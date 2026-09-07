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

use crate::client_verbs::RowLiveness;
use crate::events::EventEmitter;
// The receipt builders moved to `receipt.rs` (x-a879) so the write choke
// point (`state::update_registry`) can stage the same recovery record for a
// row removed through ANY door; re-exported so the reap path's references
// are unchanged.
use crate::codex_thread_entry::build_codex_thread_entry;
pub use crate::gc::{gc_sweep, gc_sweep_dry_run};
use crate::identity::canonical_handle;
use crate::paths::{self, AgentsHome};
use crate::protocol::{
    read_request, write_request, write_response, ErrorCode, Namespace, Request, Response,
};
pub use crate::receipt::{build_reap_receipt, write_reap_receipt, ReapReceipt};
use crate::state::{self, RegistryEntry};
use crate::AgentStatus;
use serde_json::{json, Map, Value};
use std::os::unix::fs::MetadataExt; // ino() for the bound-socket ownership check

mod list_rows;
use self::list_rows::{attention_sort_key, handle_list};
use std::os::unix::process::CommandExt; // process_group on std::process::Command
use std::path::{Path, PathBuf};
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
    /// Run one bounded reconcile sweep on daemon startup, CONCURRENTLY with the
    /// accept loop (Architecture B, plan ab-70faa65b; concurrency per x-ef7f).
    /// It used to complete before the daemon served anything, which on a large
    /// roster left a cold daemon silent for tens of seconds and had every client
    /// that timed out against that silence lazy-start another one. Default
    /// `true`; the opt-out (env `FNO_AGENTS_NO_STARTUP_RECONCILE=1`, Claude's
    /// discretion #5) skips the sweep entirely, so the first `list` reads
    /// its last recorded liveness until an idle tick settles it.
    pub reconcile_on_start: bool,
    /// cwd the idle tick resolves `agents.*` config against (retire grace,
    /// reap-receipt retention). A `Duration` cannot be pre-resolved here the
    /// way `idle_exit` is: config candidates are per-cwd, so the lookup
    /// happens at sweep time, not once at startup.
    pub agents_config_cwd: PathBuf,
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
            agents_config_cwd: PathBuf::from("."),
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
    /// Codex thread rows selected for resume by harness + full session id.
    pub recovered_threads: Vec<String>,
    pub recovery_mode: String,
    pub interrupted_write_temps: Vec<String>,
}

/// Resolve the resume identity for a daemon-hosted Codex thread.
///
/// An empty `short_id` is expected for this lane, so it cannot participate in
/// the old state-directory recovery path. The full harness session id and cwd
/// are the only durable inputs accepted for a resume.
fn codex_thread_resume_identity(
    entry: &RegistryEntry,
) -> Result<Option<(String, PathBuf)>, String> {
    if !is_codex_thread_entry(entry) {
        return Ok(None);
    }
    let session_id = entry
        .harness_session_id
        .as_deref()
        .filter(|session_id| !session_id.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "codex thread row '{}' is missing harness_session_id",
                entry.name
            )
        })?;
    if session_id.len() <= 8 || session_id.chars().any(char::is_whitespace) {
        return Err(format!(
            "codex thread row '{}' requires a full harness_session_id, got {:?}",
            entry.name, session_id
        ));
    }
    if let Some(codex_session_id) = entry.codex_session_id.as_deref() {
        if codex_session_id != session_id {
            return Err(format!(
                "codex thread row '{}' has mismatched harness_session_id and codex_session_id",
                entry.name
            ));
        }
    }
    let cwd = entry.cwd.trim();
    if cwd.is_empty() {
        return Err(format!("codex thread row '{}' is missing cwd", entry.name));
    }
    Ok(Some((session_id.to_string(), PathBuf::from(cwd))))
}

/// Whether the row was launched with the danger-full-access posture (x-de10
/// v19): the resume lane applies it so a daemon restart cannot silently demote
/// a yolo worker to workspace-write. `None` (pre-v19 rows) reads safe.
fn entry_posture_is_full_access(entry: &RegistryEntry) -> bool {
    entry.sandbox_posture.as_deref() == Some("danger-full-access")
}

fn is_codex_thread_entry(entry: &RegistryEntry) -> bool {
    entry.harness_name() == "codex"
        && entry.host_mode_or_default() == crate::state::HOST_MODE_INTERACTIVE
        && entry.short_id.is_empty()
        && entry.mux.is_none()
}

// ---------------------------------------------------------------------------
// Recovery procedure (sync, standalone-testable). Design steps 1-6; step 7
// (begin serving) is the caller's job once this returns.
// ---------------------------------------------------------------------------

/// Run the startup recovery procedure. Pure of any socket I/O so it can be
/// unit-tested against a hand-built `~/.fno/agents/` tree. The ordering
/// invariant (READ `drive_active` BEFORE clearing it, finding #12 Critical) is
/// enforced by [`crate::state::PtyState::take_active_drive`], which this calls.
///
/// Since x-4c87 an unreadable registry is a startup failure, not an empty
/// roster: `unwrap_or_default()` reads once made the daemon come up believing
/// zero agents and answer every caller from that false zero.
pub fn recover(
    home: &AgentsHome,
    emitter: &EventEmitter,
) -> Result<RecoveryReport, state::StateError> {
    recover_with_policy(home, emitter, true)
}

fn recover_with_policy(
    home: &AgentsHome,
    emitter: &EventEmitter,
    destructive: bool,
) -> Result<RecoveryReport, state::StateError> {
    let mut report = RecoveryReport {
        recovery_mode: if destructive {
            "destructive"
        } else {
            "preserve"
        }
        .into(),
        ..RecoveryReport::default()
    };
    let registry = load_registry_asserted(&home.registry_json())?;
    report.interrupted_write_temps = quarantine_interrupted_write_temps(home, emitter);

    let registered: std::collections::BTreeSet<String> = registry
        .entries
        .iter()
        .map(|e| e.short_id.clone())
        .collect();

    // Steps 2-5: per registry entry, reconcile its state.json.
    for entry in &registry.entries {
        match codex_thread_resume_identity(entry) {
            Ok(Some((_session_id, _cwd))) => {
                report.recovered_threads.push(entry.name.clone());
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = emitter.emit_fields(
                    "daemon_recovery_error",
                    json_obj(&[
                        ("op", Value::String("resume_codex_thread".into())),
                        ("name", Value::String(entry.name.clone())),
                        ("error", Value::String(error)),
                    ]),
                );
                continue;
            }
        }
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
        let is_claude_shellout = entry.harness_name() == "claude"
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
    if destructive {
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
    }

    // Step 6: orphan-PID sweep. An entry whose pid is set but is no longer OUR
    // worker is reaped; a live worker socket means the worker (Outcome B) is
    // still up. "No longer ours" = dead (ESRCH) OR a recycled pid whose start
    // time no longer matches (ab-d19e6458), else a reused pid keeps a dead
    // worker looking alive.
    let live_workers = home.scan_worker_sockets();
    let mut to_reap: Vec<(String, u32)> = Vec::new();
    for entry in &registry.entries {
        if !destructive {
            break;
        }
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
        // Keyed on (short_id, pid), not short_id alone (x-9de7 task 1). Every
        // A codex/gemini shellout row shares the same empty short_id, so a
        // short_id-only set condemns every row wearing that empty id the moment
        // ONE fails pid_is_ours. pid is what pid_is_ours verified, so it gates
        // the write.
        let reaped: std::collections::BTreeSet<(String, u32)> = to_reap.iter().cloned().collect();
        let is_reaped = |e: &RegistryEntry| {
            e.pid
                .map(|p| reaped.contains(&(e.short_id.clone(), p)))
                .unwrap_or(false)
        };
        // Ordered exit teardown (E3.3, AC-X2-4): publish any inside-leg
        // completion before the reap write clears the report below.
        for e in &registry.entries {
            if is_reaped(e) {
                emit_inside_leg_completion(emitter, e);
            }
        }
        // Surface a reap-write failure rather than silently diverging the
        // event log (which says reaped) from the on-disk registry (Gemini high).
        if let Err(e) = state::update_registry(&home.registry_json(), |r| {
            for e in r.entries.iter_mut() {
                if is_reaped(e) {
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

    Ok(report)
}

fn quarantine_interrupted_write_temps(home: &AgentsHome, emitter: &EventEmitter) -> Vec<String> {
    let mut found = Vec::new();
    let state_root = home.root().parent().unwrap_or(home.root());
    let quarantine = state_root.join(".interrupted-writes");
    for dir in [home.root(), state_root] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with('.') && (name.contains(".tmp.") || name.ends_with(".part"))) {
                continue;
            }
            let target_name = name
                .strip_prefix('.')
                .and_then(|name| name.split_once(".tmp.").map(|(target, _)| target))
                .or_else(|| {
                    name.strip_prefix('.')
                        .and_then(|name| name.strip_suffix(".part"))
                });
            let Some(target_name) = target_name else {
                continue;
            };
            let target = dir.join(target_name);
            let Ok(Some(_lock)) = state::try_lock_path_exclusive(&target) else {
                continue;
            };
            if !entry.path().exists() {
                continue;
            }
            let _ = std::fs::create_dir_all(&quarantine);
            let dest = quarantine.join(format!("{}-{}", now_compact(), name));
            let outcome = if std::fs::rename(entry.path(), &dest).is_ok() {
                "quarantined"
            } else {
                "detected"
            };
            let _ = emitter.emit(
                "daemon_recovery_interrupted_temp",
                &json!({"name": name, "outcome": outcome, "quarantined_to": dest}),
            );
            found.push(name);
        }
    }
    found
}

/// A live process's start time, used to distinguish "our worker" from a recycled
/// PID (ab-d19e6458). `None` if the process is gone or the lookup is
/// unsupported/failed. The value is a per-host, per-boot quantity compared only
/// for equality against a value captured for the SAME pid, so the differing
/// units across platforms (Linux ticks vs macOS microseconds) do not matter.
///
/// DO NOT read this as a wall clock. At least three writers fill the column it
/// lands in, in at least three conventions: this function (Linux ticks / macOS
/// micros), `_process_start_time` in cli/src/fno/agents/spawn_gate.py, and
/// `claude_adopt.rs`, which passes through whatever claude's own roster wrote.
/// Converting one of them to epoch time makes the equality comparisons in
/// `pid_is_ours` and `_pid_alive` fail across writers, which reaps live workers.
/// A consumer that needs a real start time needs its own field, not this token.
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

/// Distinct canonical repo roots the registry knows about, deduplicated.
///
/// A linked worktree is not its own repo, so its rows fold into the checkout
/// that owns them and the sweep runs once per repo rather than once per row.
fn registry_repo_roots(home: &AgentsHome) -> Vec<String> {
    let Ok(loaded) = state::load_registry(&home.registry_json()) else {
        return Vec::new();
    };
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for e in &loaded.entries {
        let root = if e.project_root.is_empty() {
            e.cwd.clone()
        } else {
            e.project_root.clone()
        };
        if !root.is_empty() && std::path::Path::new(&root).is_dir() {
            seen.insert(root);
        }
    }
    if let Ok(contents) = std::fs::read_to_string(home.events_jsonl()) {
        for line in contents.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if event.get("type").and_then(Value::as_str) != Some("merge_cleanup_requested") {
                continue;
            }
            let Some(repo) = event
                .get("data")
                .and_then(|data| data.get("repo"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if std::path::Path::new(repo).is_dir() {
                seen.insert(repo.to_string());
            }
        }
    }
    seen.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeCleanupRequest {
    request_id: String,
    repo: String,
    pr: i64,
    worktree: Option<String>,
    node_ids: Vec<String>,
    candidate_row_names: Vec<String>,
}

fn pending_merge_cleanup_requests(home: &AgentsHome, repo: &str) -> Vec<MergeCleanupRequest> {
    let Ok(contents) = std::fs::read_to_string(home.events_jsonl()) else {
        return Vec::new();
    };
    let mut requested = std::collections::BTreeMap::<String, MergeCleanupRequest>::new();
    let mut finished = std::collections::HashSet::<String>::new();
    for line in contents.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(data) = event.get("data") else {
            continue;
        };
        let Some(request_id) = data.get("request_id").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "merge_cleanup_requested" => {
                let Some(request_repo) = data.get("repo").and_then(Value::as_str) else {
                    continue;
                };
                if request_repo != repo {
                    continue;
                }
                let Some(pr) = data.get("pr").and_then(Value::as_i64) else {
                    continue;
                };
                let node_ids = data
                    .get("node_ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let candidate_row_names = data
                    .get("candidate_row_names")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                requested.insert(
                    request_id.to_owned(),
                    MergeCleanupRequest {
                        request_id: request_id.to_owned(),
                        repo: request_repo.to_owned(),
                        pr,
                        worktree: data
                            .get("worktree")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        node_ids,
                        candidate_row_names,
                    },
                );
            }
            "merge_cleanup_completed" | "merge_cleanup_refused" => {
                finished.insert(request_id.to_owned());
            }
            _ => {}
        }
    }
    requested
        .into_values()
        .filter(|request| !finished.contains(&request.request_id))
        .collect()
}

fn merge_cleanup_requested(home: &AgentsHome, repo: &str) -> bool {
    !pending_merge_cleanup_requests(home, repo).is_empty()
}

fn merge_cleanup_reclaimed_bytes(home: &AgentsHome, worktree: &str) -> u64 {
    let Ok(contents) = std::fs::read_to_string(home.events_jsonl()) else {
        return 0;
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("worktree_removed"))
        .filter_map(|event| event.get("data").cloned())
        .filter(|data| data.get("path").and_then(Value::as_str) == Some(worktree))
        .filter_map(|data| data.get("reclaimed_bytes").and_then(Value::as_u64))
        .last()
        .unwrap_or(0)
}

fn merge_cleanup_guard_reason(repo: &str, worktree: &str) -> Option<String> {
    let status = std::process::Command::new("git")
        .current_dir(worktree)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !status.status.success() {
        return Some("git-status-unreadable".into());
    }
    if !status.stdout.is_empty() {
        return Some("dirty".into());
    }
    let origin = std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", "--quiet", "origin/main"])
        .output()
        .ok()?;
    if !origin.status.success() {
        return Some("origin-main-unreadable".into());
    }
    let merged = std::process::Command::new("git")
        .current_dir(worktree)
        .args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
        .status()
        .ok()?;
    if !merged.success() {
        return Some("unreachable-from-origin-main".into());
    }
    None
}

fn merge_cleanup_row_names(home: &AgentsHome, request: &MergeCleanupRequest) -> Vec<String> {
    let Ok(registry) = state::load_registry(&home.registry_json()) else {
        return Vec::new();
    };
    let mut names = request.candidate_row_names.clone();
    names.extend(
        registry
            .entries
            .into_iter()
            .filter(|entry| {
                request
                    .worktree
                    .as_deref()
                    .is_some_and(|worktree| entry.cwd == worktree)
                    || request
                        .node_ids
                        .iter()
                        .any(|node| entry.name.starts_with(&format!("target-{node}-")))
            })
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
    );
    names.sort();
    names.dedup();
    names
}

fn consume_merge_cleanup_requests(home: &AgentsHome, roots: &[String], emitter: &EventEmitter) {
    for root in roots {
        for request in pending_merge_cleanup_requests(home, root) {
            if let Some(worktree) = request.worktree.as_deref() {
                if std::path::Path::new(worktree).exists() {
                    if let Some(reason) = merge_cleanup_guard_reason(root, worktree) {
                        let _ = emitter.emit(
                            "merge_cleanup_refused",
                            &json!({
                                "request_id": request.request_id,
                                "repo": request.repo,
                                "pr": request.pr,
                                "reason": reason,
                            }),
                        );
                    }
                    continue;
                }
            }
            let reclaimed_bytes = request
                .worktree
                .as_deref()
                .map(|worktree| merge_cleanup_reclaimed_bytes(home, worktree))
                .unwrap_or(0);
            let names = merge_cleanup_row_names(home, &request);
            let mut failed = None;
            for name in names {
                let output = std::process::Command::new("fno")
                    .current_dir(root)
                    .args([
                        "agents",
                        "rm",
                        &name,
                        "--audit-actor",
                        "post-merge",
                        "--audit-reason",
                        "pr-merged",
                        "--audit-request-id",
                        &request.request_id,
                        "--audit-worktree-touched",
                        "--audit-reclaimed-bytes",
                        &reclaimed_bytes.to_string(),
                    ])
                    .output();
                if !output.as_ref().is_ok_and(|output| output.status.success()) {
                    failed = Some(name);
                    break;
                }
            }
            if let Some(name) = failed {
                let _ = emitter.emit(
                    "merge_cleanup_refused",
                    &json!({
                        "request_id": request.request_id,
                        "repo": request.repo,
                        "pr": request.pr,
                        "reason": format!("row-removal-failed:{name}"),
                    }),
                );
                continue;
            }
            let _ = emitter.emit(
                "merge_cleanup_completed",
                &json!({
                    "request_id": request.request_id,
                    "repo": request.repo,
                    "pr": request.pr,
                    "reclaimed_bytes": reclaimed_bytes,
                }),
            );
        }
    }
}

/// How long between worktree report sweeps. A 24-hour reap order spans at
/// least three complete windows even when its mint cannot clear the stamp.
const WORKTREE_SWEEP_INTERVAL_SECS: u64 = 21_600;

/// How long between stale-question reconciles. Stale rows are measured in
/// hundreds of hours, so the interval bounds discovery lag, not freshness:
/// a row that crosses the wake ceiling waits at most one interval before a
/// human is told. Identity-keyed dedupe lives in the verb, so an eager run
/// costs one sweep and changes nothing.
const STALE_SWEEP_INTERVAL_SECS: i64 = 21_600;

/// One fleet's stale-sweep reading, parsed from the verb's JSON line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleSweepReport {
    pub stale: usize,
    pub oldest_h: i64,
    pub outcome: String,
}

/// Parse the JSON object `fno agents stale-escalate --json` prints on stdout.
///
/// The scheduled invocation passes `--json`, so stdout is ONE JSON line whose
/// `summary` field happens to carry a `Summary: ...` string - the line itself
/// never starts with it. Parse the object's fields, not that embedded text.
///
/// Returns `None` rather than a zeroed report when no readable object is
/// present. A sweep that could not read its own output must not report
/// "0 stale", which is indistinguishable from a clean machine: an absence has
/// two explanations and a count must only ever come from a real reading. The
/// outcome word rides along because on the refused path the count is NOT a
/// real reading - the event must be able to say so rather than fabricate a
/// measured zero.
pub fn parse_stale_sweep(stdout: &str) -> Option<StaleSweepReport> {
    let line = stdout
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with('{'))?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(StaleSweepReport {
        stale: usize::try_from(value.get("stale_count")?.as_u64()?).ok()?,
        oldest_h: value.get("oldest_h")?.as_i64()?,
        outcome: value.get("outcome")?.as_str()?.to_string(),
    })
}

/// Stale-question reconcile on a 6h floor: report-only, no apply mode.
///
/// Rows past the wake ceiling are the watchdog's needs-human bucket - no
/// action lane may take them - so the durable question channel is the only
/// surface they reach. This sweep is its trigger; the verb inside reconciles
/// one question to the measured set, so a re-run is a duplicate no-op unless
/// the set changed. Removal stays everywhere it already was: this fn takes no
/// apply flag and shells no action verb, and the run closure is injected so
/// the policy is testable without shelling out.
///
/// Emits one `stale_sweep` event per run, INCLUDING on outcome `none` or
/// `duplicate`: a tick that stays silent when it finds nothing cannot be told
/// from a tick that never ran.
pub fn stale_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    run: &dyn Fn() -> Option<String>,
) -> usize {
    let stamp = home.root().join("stale-escalate.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < STALE_SWEEP_INTERVAL_SECS {
        return 0;
    }
    let outcome = match run().as_deref().and_then(parse_stale_sweep) {
        Some(r) => {
            let _ = emitter.emit(
                "stale_sweep",
                &json!({
                    "stale_count": r.stale,
                    "oldest_h": r.oldest_h,
                    "outcome": r.outcome,
                }),
            );
            1
        }
        None => {
            let _ = emitter.emit("stale_sweep", &json!({"error": "unreadable-summary"}));
            0
        }
    };
    let _ = std::fs::write(&stamp, now.to_string());
    outcome
}

/// One repo's worktree-sweep reading, parsed from the verb's `Summary:` line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeSweepReport {
    pub eligible: usize,
    pub kept: usize,
    pub dirty: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSweepOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSweepOrderRead {
    pub standing: Option<bool>,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl From<bool> for WorktreeSweepOrderRead {
    fn from(standing: bool) -> Self {
        Self {
            standing: Some(standing),
            exit_code: Some(0),
            stderr: String::new(),
        }
    }
}

/// Parse `fno agents workspace worktree cleanup --merged`'s summary line.
///
/// Returns `None` rather than a zeroed report when the line is absent. A sweep
/// that could not read its own output must not report "0 eligible, 0 dirty",
/// which is indistinguishable from a clean machine: an absence has two
/// explanations and a count must only ever come from a real reading.
///
/// The verb differs by mode (`would archive` dry-run vs `archived` apply), so
/// the eligible count reads from whichever the line carries.
pub fn parse_worktree_sweep(stdout: &str) -> Option<WorktreeSweepReport> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("Summary:"))?;
    let num_before = |needle: &str| -> Option<usize> {
        let idx = line.find(needle)?;
        line[..idx].split_whitespace().last()?.parse().ok()
    };
    let eligible = num_before(" would archive").or_else(|| num_before(" archived"))?;
    Some(WorktreeSweepReport {
        eligible,
        kept: num_before(" kept (")?,
        dirty: num_before(" dirty")?,
    })
}

/// Worktree sweep, one line per repo, on a 6h floor: report-only until a
/// merge-minted reap order stands, then applying.
///
/// A timer tick proves nothing on its own, so an unearned tick still only
/// REPORTS. Removal stays on the merge-triggered path: the post-merge ritual
/// mints a `reap:pr-<n>` claim (TTL-bounded) before archive lookup, and while
/// any such order stands in a repository (`orders` injects that scoped read)
/// that repository's pass runs with `--apply`. The sweep's own guards -
/// reapable, live claim, rooted processes - decide tree by tree. A tree that
/// stays protected expires its order rather than being forced. There is no
/// config knob, because two off-switches for one decision strand whoever
/// flips the wrong one.
///
/// `orders` and `run` are injected so the policy is testable without shelling
/// out.
pub fn worktree_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    roots: &[String],
    orders: &dyn Fn(&str) -> WorktreeSweepOrderRead,
    run: &dyn Fn(&str, bool) -> WorktreeSweepOutput,
) -> usize {
    let stamp = home.root().join("worktree-sweep.stamp");
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < WORKTREE_SWEEP_INTERVAL_SECS as i64 {
        return 0;
    }
    let mut swept = 0;
    for root in roots {
        let order_read = orders(root);
        let Some(apply) = order_read.standing else {
            let stderr = order_read.stderr.lines().next().unwrap_or("");
            let _ = emitter.emit(
                "worktree_sweep",
                &json!({
                    "repo": root,
                    "error": "unreadable-orders",
                    "exit_code": order_read.exit_code,
                    "stderr": stderr,
                }),
            );
            continue;
        };
        let mode = if apply { "apply-orders" } else { "report-only" };
        // Emit for EVERY repo, including the ones that read zero. A tick that
        // stays silent when it finds nothing cannot be told from a tick that
        // never ran, and this sweep exists precisely to surface what the
        // ritual missed.
        let output = run(root, apply);
        let report = (output.exit_code == Some(0))
            .then(|| parse_worktree_sweep(&output.stdout))
            .flatten();
        match report {
            Some(r) => {
                let _ = emitter.emit(
                    "worktree_sweep",
                    &json!({
                        "repo": root,
                        "eligible": r.eligible,
                        "kept": r.kept,
                        "dirty": r.dirty,
                        "mode": mode,
                    }),
                );
                swept += 1;
            }
            None => {
                let stderr = output.stderr.lines().next().unwrap_or("");
                let _ = emitter.emit(
                    "worktree_sweep",
                    &json!({
                        "repo": root,
                        "mode": mode,
                        "error": "unreadable-summary",
                        "exit_code": output.exit_code,
                        "stderr": stderr,
                    }),
                );
            }
        }
    }
    let _ = std::fs::write(&stamp, now.to_string());
    swept
}

/// One per-sweep index of the harness transcript stores. A registry-wide
/// question ("which rows' sessions still exist in their own store") is answered
/// by ONE walk per harness and in-memory lookups, never a walk per row: the
/// first cut of this walked `~/.claude/projects` twice per past-grace row, and
/// a live dry-run against a 125-row registry outran any operator's patience.
///
/// `None` from [`HarnessStoreIndex::matches`] means "cannot judge": no session
/// id recorded, an unknown harness, or a store directory that could not be
/// read (AC5 fail-closed - an unreadable store has two explanations and only
/// one of them is a dead session).
///
/// claude: `~/.claude/projects/*/<session_id>*.jsonl`, across EVERY project
/// dir because a session's transcript can live in more than one (EnterWorktree
/// re-keys it; the other dir keeps a stub). codex: the rollout jsonl under
/// `~/.codex/sessions/` embedding the session id in its filename - the same
/// shape `fno.agents.discover.codex_rollout_for_session` resolves. A harness a
/// reaper cannot speak for is NEVER judged by another harness's store (AC3): a
/// codex row has no claude transcript by construction, so a claude-keyed probe
/// would reap every codex worker on the machine.
#[derive(Default)]
pub(crate) struct HarnessStoreIndex {
    /// Resolved store roots; `None` until the first lookup resolves them from
    /// `$HOME` (or forever, for an index built `with_roots` in tests).
    claude_root: Option<std::path::PathBuf>,
    codex_root: Option<std::path::PathBuf>,
    /// `(filename, path)` for every candidate file, or `None` until the first
    /// lookup walks the store. `Some(Err(()))` marks a walk that hit an
    /// unreadable directory: every later lookup answers None, fail closed.
    claude: Option<Result<Vec<(String, std::path::PathBuf)>, ()>>,
    codex: Option<Result<Vec<(String, std::path::PathBuf)>, ()>>,
}

impl HarnessStoreIndex {
    /// Test seam: fixed roots, so the per-harness keying is unit-testable
    /// against temp trees instead of the developer's real `~/.claude`/`~/.codex`.
    /// (Called only from the lib test suite. Deliberately NOT gated with the
    /// test cfg attribute: the emit-kind scanner in lib.rs truncates each file
    /// at the first byte-level occurrence of that attribute's text, so a
    /// mid-file gate would classify every later production emit as test-only.)
    #[allow(dead_code)]
    fn with_roots(claude_root: std::path::PathBuf, codex_root: std::path::PathBuf) -> Self {
        HarnessStoreIndex {
            claude_root: Some(claude_root),
            codex_root: Some(codex_root),
            ..Default::default()
        }
    }

    fn root(&self, harness: &str) -> Option<std::path::PathBuf> {
        let slot = match harness {
            "claude" => &self.claude_root,
            "codex" => &self.codex_root,
            _ => return None,
        };
        slot.clone().or_else(|| {
            let home = std::path::PathBuf::from(std::env::var("HOME").ok()?);
            match harness {
                "claude" => Some(home.join(".claude").join("projects")),
                // Resolve the codex home the way codex itself does, so a
                // CODEX_HOME redirect never reads this reaper into a store
                // the worker never wrote (an empty wrong-store read would
                // read as "session gone" - death evidence from an absence).
                _ => crate::client_verbs::codex_home().map(|h| h.join("sessions")),
            }
        })
    }

    /// Every transcript candidate this row's harness store holds for its
    /// session id. Empty vector = the session is GONE from its own store.
    pub(crate) fn matches(&mut self, e: &state::RegistryEntry) -> Option<Vec<std::path::PathBuf>> {
        let sid = e.harness_session_id.as_deref().filter(|s| !s.is_empty())?;
        let harness = e.harness_name();
        let root = match harness {
            "claude" | "codex" => self.root(harness)?,
            // Unknown/unsupported harness (gemini, opencode, ...): no store
            // this reaper can read. Answer None, never another harness's store.
            _ => return None,
        };
        let cached_empty = match harness {
            "claude" => self.claude.is_none(),
            _ => self.codex.is_none(),
        };
        if cached_empty {
            // First lookup for this harness: one walk, cached for the sweep
            // (an unreadable store caches as Err, so it stays fail-closed
            // for every later row too instead of re-walking per row).
            let indexed = index_tree(&root, 0);
            let parked = match harness {
                "claude" => &mut self.claude,
                _ => &mut self.codex,
            };
            *parked = Some(indexed);
        }
        let files = match harness {
            "claude" => self.claude.as_ref()?,
            _ => self.codex.as_ref()?,
        };
        let files = files.as_ref().ok()?;
        Some(
            files
                .iter()
                .filter(|(name, _)| match harness {
                    // `<uuid>.jsonl` and its stub artifacts (`<uuid>.orphaned-...`)
                    // all prove the session still EXISTS in the store; which of
                    // them carries conversation is a content question this
                    // existence probe does not need to answer.
                    "claude" => name.starts_with(sid) && name.ends_with(".jsonl"),
                    _ => crate::client_verbs::codex_rollout_matches(&name, sid),
                })
                .map(|(_, p)| p.clone())
                .collect(),
        )
    }
}

/// Wall-clock bound for one harness removal subprocess (`run_claude_rm`). A
/// hung removal must never wedge its caller (the operator measured a 300s+
/// hang on a stuck row; the removal cannot inherit it).
const CASCADE_TIMEOUT: Duration = Duration::from_secs(15);

/// Bounded walk collecting `(filename, path)` for every regular file under
/// `dir` (claude is two levels, codex four; depth 5 covers both). `Err` on any
/// unreadable directory: an unreadable store answers nothing, fail closed.
pub(crate) fn index_tree(
    dir: &std::path::Path,
    depth: usize,
) -> Result<Vec<(String, std::path::PathBuf)>, ()> {
    let mut out = Vec::new();
    if depth > 5 {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| ())?;
        if file_type.is_dir() {
            out.extend(index_tree(&path, depth + 1)?);
        } else {
            out.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }
    Ok(out)
}

/// Remove a reaped row's session from its OWN harness's store (AC6). Returns
/// `Some((row_id, reason))` when harness removal refused or failed; `None` on
/// success, verified absence, or a registry-only harness. Garbage collection
/// calls this after its registry reap; explicit `rm` uses the detailed outcome
/// before its registry write so a failure stays retryable.
///
/// claude: `claude rm <short_id>` - the same surface `fno agents rm` shells
/// out to, bounded here by CASCADE_TIMEOUT, when `claude agents --json --all`
/// still sees the row or that list is unreadable. codex: drop the session's
/// entry from `~/.codex/session_index.jsonl`
/// (transcript files stay; this is the index record, matching the Python rm
/// teardown arm). opencode/gemini: registry-only by contract - nothing to
/// cascade.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CascadeOutcome {
    Removed,
    AlreadyAbsent(String),
    Unverified(String),
    Failed(String),
    NotApplicable,
}

impl CascadeOutcome {
    fn removed_json(&self) -> Value {
        match self {
            Self::Removed => Value::Bool(true),
            Self::AlreadyAbsent(_) | Self::Failed(_) => Value::Bool(false),
            Self::Unverified(_) | Self::NotApplicable => Value::Null,
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::AlreadyAbsent(reason) | Self::Unverified(reason) | Self::Failed(reason) => {
                Some(reason)
            }
            Self::Removed | Self::NotApplicable => None,
        }
    }
}

fn claude_row_id(e: &state::RegistryEntry) -> Option<String> {
    if !e.short_id.is_empty() {
        return Some(e.short_id.clone());
    }
    e.harness_session_id
        .as_deref()
        .filter(|session_id| !session_id.is_empty())
        .map(|session_id| session_id.chars().take(8).collect())
}

/// True only when a KNOWN roster snapshot was consulted and the row is not
/// in it. A `None`/unknown snapshot proves nothing, so it is never absent on
/// that basis alone. The single predicate both the pre-cascade live-gate and
/// the cascade's own already-absent check apply, so "what counts as absent"
/// cannot diverge between the two call sites.
fn claude_row_provably_absent(
    claude_agents: Option<&crate::claude_roster::ClaudeAgentsSnapshot>,
    row_id: Option<&str>,
) -> bool {
    claude_agents
        .is_some_and(|snap| snap.is_known() && row_id.is_some_and(|id| snap.find(id).is_none()))
}

fn cascade_harness_session_result_with(
    e: &state::RegistryEntry,
    claude_agents: Option<&crate::claude_roster::ClaudeAgentsSnapshot>,
    read_claude_agents: &dyn Fn() -> crate::claude_roster::ClaudeAgentsSnapshot,
    claude_rm: &dyn Fn(&str) -> Result<(), String>,
) -> CascadeOutcome {
    let row_id = claude_row_id(e).unwrap_or_else(|| e.name.clone());
    match e.harness_name() {
        "claude" => {
            let Some(short_id) = claude_row_id(e) else {
                return CascadeOutcome::Failed(
                    "claude cascade has no short id and no session id".into(),
                );
            };
            let snapshot = claude_agents.expect("Claude cascade requires an agent-list snapshot");
            if claude_row_provably_absent(Some(snapshot), Some(&short_id)) {
                return CascadeOutcome::AlreadyAbsent(format!(
                    "claude row {short_id} already absent"
                ));
            }
            if let Err(reason) = claude_rm(&short_id) {
                return CascadeOutcome::Failed(reason);
            }
            let after = read_claude_agents();
            if after.find(&short_id).is_some() {
                return CascadeOutcome::Failed(format!(
                    // retired-ok: reports a successful shellout that left the row behind.
                    "claude row {short_id} survives successful claude rm"
                ));
            }
            match &after {
                crate::claude_roster::ClaudeAgentsSnapshot::Known { .. } => CascadeOutcome::Removed,
                crate::claude_roster::ClaudeAgentsSnapshot::Unknown { .. } => {
                    CascadeOutcome::Unverified(format!(
                        "claude post-removal list unreadable: {}",
                        after.warning_text()
                    ))
                }
            }
        }
        "codex" => {
            let Some(sid) = e.harness_session_id.as_deref() else {
                return CascadeOutcome::NotApplicable;
            };
            let index = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".codex")
                .join("session_index.jsonl");
            match cascade_codex_index(&index, sid, &row_id) {
                Ok(true) => CascadeOutcome::Removed,
                Ok(false) => CascadeOutcome::AlreadyAbsent("codex index row already absent".into()),
                Err((_, reason)) => CascadeOutcome::Failed(reason),
            }
        }
        "cursor-agent" => {
            // rm runs after the liveness gate, so the owner pid is normally
            // already gone and the census's ownership proof is unprovable by
            // construction. Failed would brick every post-stop rm; the row
            // goes and the leak, if any, is named. A reap that fails with
            // handles in hand is different - servers are provably alive and
            // surviving - and still fails the removal.
            let handles =
                match crate::cursor_agent::capture_detached_worker_servers(e.pid, e.pid_start_time)
                {
                    Ok(handles) => handles,
                    Err(reason) => {
                        return CascadeOutcome::Unverified(format!(
                            "worker-server cleanup unverified: {reason}"
                        ))
                    }
                };
            match crate::cursor_agent::reap_detached_worker_servers(&handles) {
                Ok(0) => CascadeOutcome::AlreadyAbsent(
                    "cursor-agent worker-server was already absent".into(),
                ),
                Ok(_count) => CascadeOutcome::Removed,
                Err(reason) => CascadeOutcome::Failed(reason),
            }
        }
        _ => CascadeOutcome::NotApplicable,
    }
}

fn run_claude_rm(short_id: &str) -> Result<(), String> {
    let mut child = std::process::Command::new("claude")
        .args(["rm", short_id])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("claude rm failed to start: {error}"))?;
    let deadline = std::time::Instant::now() + CASCADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                let output = child.wait_with_output().ok();
                let detail = output
                    .as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stderr))
                    .unwrap_or_default();
                // retired-ok: reports the shellout this code ran and its exit code; tells no reader to run it.
                return Err(format!("claude rm exited {code}: {}", detail.trim()));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("claude rm timed out".into());
            }
            Err(error) => return Err(format!("claude rm wait failed: {error}")),
        }
    }
}

/// The codex cascade core: drop this session's entry from the session index
/// (the index RECORD, never the rollout transcript). Path-injected so the
/// surgery is unit-testable against a temp index. `None` = nothing to do
/// (missing index, or no matching entry); `Some((row_id, reason))` = refusal
/// or failure to surface, never a swallowed error.
fn cascade_codex_index(
    index: &std::path::Path,
    sid: &str,
    row_id: &str,
) -> Result<bool, (String, String)> {
    let text = match std::fs::read_to_string(index) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err((row_id.to_string(), format!("codex index unreadable: {err}"))),
    };
    // Index line surgery: drop only lines whose parsed session id equals this
    // row's. A line that fails to parse stays (never destroy the index to
    // clean it).
    let kept: Vec<&str> = text
        .lines()
        .filter(
            |line| match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => v.get("session_id").and_then(|s| s.as_str()) != Some(sid),
                Err(_) => true,
            },
        )
        .collect();
    if kept.len() == text.lines().count() {
        return Ok(false);
    }
    let mut rewritten = kept.join("\n");
    if !rewritten.is_empty() {
        rewritten.push('\n');
    }
    match std::fs::write(index, rewritten) {
        Ok(()) => Ok(true),
        Err(err) => Err((
            row_id.to_string(),
            format!("codex index write failed: {err}"),
        )),
    }
}

/// Is `cwd` a LINKED git worktree, as opposed to the canonical checkout or a
/// plain directory?
///
/// A linked worktree's `.git` is a FILE containing a `gitdir:` pointer; the
/// canonical checkout's `.git` is a directory. That difference is the whole
/// test, it needs no subprocess, and it is what separates a row that owns
/// something removable from one that merely ran somewhere.
///
/// Fails closed in the useful direction: a path we cannot read is "owns
/// nothing", so its row is judged on terminal status and grace alone rather
/// than pinned forever by a cleanliness answer that could never arrive.
pub(crate) fn is_linked_worktree(cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    std::path::Path::new(cwd).join(".git").is_file()
}

/// Can this worktree-owning row's `cwd` be removed without destroying work?
/// `Some(true)` yes, `Some(false)` no, `None` the probe could not determine it
/// -> the caller fails closed and keeps the row.
///
/// Routes through `fno agents workspace worktree reapable`, the same answer the `--merged` sweep
/// and `archive-worktree.sh` use, so three call sites cannot drift apart (an
/// equivalence test pins that they agree). The old rule here was "is
/// `git status --porcelain` empty", which blocked on a tracked file merely
/// MISSING from disk - content HEAD still holds, so removal loses nothing.
///
/// Permission needs BOTH a clean exit and the literal `reapable=yes` marker. A
/// stale `fno` predating the verb exits non-zero with no receipt, which is
/// indistinguishable from any other non-answer, so every unknown degrades to
/// `None` and the row is kept. That is exactly the prior behaviour.
pub(crate) fn worktree_clean_probe(cwd: &str) -> Option<bool> {
    let out = std::process::Command::new("fno")
        .current_dir(cwd)
        .args(["agents", "workspace", "worktree", "reapable", cwd])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        // Never read a bare exit 0 as permission: an empty stdout (a shim that
        // swallowed the verb) would otherwise reap a live worktree.
        return if text.contains("reapable=yes") {
            Some(true)
        } else {
            None
        };
    }
    if text.contains("reapable=no") {
        return Some(false);
    }
    None
}

/// (x-d545) The reapable gate's answer for a removed row's worktree: the
/// verdict, plus the reason a kept tree names in its receipt.
enum WorktreeGate {
    Reapable,
    Blocked(String),
    Unanswerable(String),
}

/// Per-subprocess budget for the rm worktree path, matching the Python
/// runtime's remove bound (`subprocess.run(..., timeout=60.0)`).
const RM_SUBPROCESS_TIMEOUT_SECS: u64 = 60;

/// One subprocess read under a wall-clock budget: `std` has no
/// `Command::output` timeout, and a git stalled on a wedged filesystem must
/// not park the daemon's rm handler forever. Past the deadline the child is
/// killed and the killed status returned, so a "kept" receipt can never be
/// contradicted by a removal finishing in the background.
fn output_with_timeout(mut cmd: std::process::Command, secs: u64) -> Option<std::process::Output> {
    use std::io::Read;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut out);
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut err);
        }
        (out, err)
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                break child.wait().ok()?;
            }
            Err(_) => return None,
        }
    };
    let (stdout, stderr) = reader.join().ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Is the worktree's branch merged into the repo's main line? The rm door's
/// half of the third bucket: the `--merged` sweep merge-filters BEFORE its
/// gate, and this caller has no such pre-filter, so it asks here. `None`:
/// nothing names the work or the main line (detached HEAD, no main ref, git
/// error) - the caller keeps the tree. Mirrors
/// `fno.worktree_reapable.branch_merged`, the Python door's same question.
pub(crate) fn branch_merged(cwd: &str) -> Option<bool> {
    let mut bases = vec!["origin/main".to_string(), "main".to_string()];
    if let Some(out) = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(cwd)
                .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
            cmd
        },
        RM_SUBPROCESS_TIMEOUT_SECS,
    ) {
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !head.is_empty() {
            bases.insert(0, head);
        }
    }
    let mut base: Option<String> = None;
    for candidate in &bases {
        let known = output_with_timeout(
            {
                let mut cmd = std::process::Command::new("git");
                cmd.current_dir(cwd)
                    .args(["rev-parse", "--verify", "--quiet", candidate]);
                cmd
            },
            RM_SUBPROCESS_TIMEOUT_SECS,
        );
        if known.is_some_and(|out| out.status.success()) {
            base = Some(candidate.clone());
            break;
        }
    }
    let base = base?;
    let branch = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(cwd).args(["branch", "--show-current"]);
            cmd
        },
        RM_SUBPROCESS_TIMEOUT_SECS,
    )?;
    if !branch.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    if branch.is_empty() {
        return None;
    }
    let merged = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(cwd)
                .args(["merge-base", "--is-ancestor", &branch, &base]);
            cmd
        },
        RM_SUBPROCESS_TIMEOUT_SECS,
    )?;
    match merged.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Ask `fno agents workspace worktree reapable` - the same verb the `--merged`
/// sweep, `archive-worktree.sh` and the GC probe ask - and read BOTH the
/// literal marker and the reason, so a kept tree's receipt can name why. A
/// `yes` then meets the merge check, because this door has no sweep-style
/// pre-filter: a clean-but-unmerged branch is exactly where abandoned-but-real
/// work lives, and the contract keeps it for a human.
fn worktree_gate(cwd: &str) -> WorktreeGate {
    let out = match output_with_timeout(
        {
            let mut cmd = std::process::Command::new("fno");
            cmd.args(["agents", "workspace", "worktree", "reapable", cwd]);
            cmd
        },
        RM_SUBPROCESS_TIMEOUT_SECS,
    ) {
        Some(out) => out,
        None => {
            return WorktreeGate::Unanswerable("the reapable probe could not run".into());
        }
    };
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let reason = |fallback: &str| -> String {
        text.split("reason=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap_or(fallback)
            .to_string()
    };
    // The literal marker, never a bare exit code a shim could swallow - the
    // same double permission `worktree_clean_probe` needs.
    if out.status.success() && text.contains("reapable=yes") {
        return match branch_merged(cwd) {
            Some(true) => WorktreeGate::Reapable,
            Some(false) => WorktreeGate::Blocked("clean but the branch is not merged".into()),
            None => WorktreeGate::Unanswerable("the merged-branch probe could not answer".into()),
        };
    }
    if text.contains("reapable=no") {
        return WorktreeGate::Blocked(reason("blocked"));
    }
    WorktreeGate::Unanswerable("the reapable probe could not answer".into())
}

/// (x-d545) A human removed ONE named row: its worktree goes with it, but
/// only through the reapable gate plus the merge check - the same three
/// buckets the `--merged` sweep and the watchdog honor (DIRTY untouched,
/// clean-and-unmerged never auto-pruned, clean-and-MERGED loses the TREE and
/// keeps the BRANCH: `git worktree remove` never deletes branches). A gate
/// that cannot answer keeps the tree - removal never guesses. The row is
/// removed either way; a protected worktree must not wedge the row on the
/// sideline. `None`: the row owned no linked worktree, a clean no-op.
fn rm_take_worktree_with(
    entry: &state::RegistryEntry,
    gate: &dyn Fn(&str) -> WorktreeGate,
    remove: &dyn Fn(&str) -> Result<(), String>,
) -> Option<String> {
    let cwd = entry.cwd.as_str();
    if !is_linked_worktree(cwd) {
        return None;
    }
    match gate(cwd) {
        WorktreeGate::Reapable => match remove(cwd) {
            Ok(()) => Some(format!("worktree removed: {cwd}")),
            Err(e) => Some(format!(
                "worktree kept: {cwd} (git worktree remove failed: {e})"
            )),
        },
        WorktreeGate::Blocked(reason) => {
            Some(format!("worktree kept: {cwd} (the gate said no: {reason})"))
        }
        WorktreeGate::Unanswerable(why) => Some(format!("worktree kept: {cwd} ({why})")),
    }
}

pub(crate) fn rm_take_worktree(entry: &state::RegistryEntry) -> Option<String> {
    rm_take_worktree_with(entry, &worktree_gate, &|cwd| {
        // Run git FROM the worktree: the daemon's own cwd is usually not a
        // repository, and `git worktree remove` needs one to resolve against.
        // A forced self-removal from inside the leaf is allowed by git.
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(cwd)
            .args(["worktree", "remove", "--force", cwd]);
        output_with_timeout(cmd, RM_SUBPROCESS_TIMEOUT_SECS)
            .ok_or_else(|| "the removal timed out".to_string())
            .and_then(|out| {
                if out.status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "exited {}: {}",
                        out.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))
                }
            })
    })
}

#[derive(Debug, Clone)]
struct RemovalAuditContext {
    actor: String,
    reason: String,
    request_id: String,
    worktree_touched: Option<bool>,
    reclaimed_bytes: Option<u64>,
}

impl RemovalAuditContext {
    fn from_request(req: &Request, entry: &state::RegistryEntry) -> Self {
        let string_param = |key: &str| {
            req.params
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        };
        let actor = string_param("audit_actor").unwrap_or_else(|| {
            [
                "FNO_HARNESS_SESSION_ID",
                "CLAUDE_CODE_SESSION_ID",
                "CODEX_THREAD_ID",
            ]
            .iter()
            .find_map(|key| std::env::var(key).ok())
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("session:{value}"))
            .unwrap_or_else(|| "operator".into())
        });
        let reason = string_param("audit_reason").unwrap_or_else(|| "operator-requested".into());
        let request_id = string_param("audit_request_id")
            .unwrap_or_else(|| format!("agent-rm:{}:{}", entry.name, entry.created_at));
        Self {
            actor,
            reason,
            request_id,
            worktree_touched: req
                .params
                .get("audit_worktree_touched")
                .and_then(Value::as_bool),
            reclaimed_bytes: req
                .params
                .get("audit_reclaimed_bytes")
                .and_then(Value::as_u64),
        }
    }
}

fn directory_bytes(path: &std::path::Path) -> Option<u64> {
    fn walk(path: &std::path::Path, total: &mut u64) -> std::io::Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                walk(&entry.path(), total)?;
            } else {
                *total = total.saturating_add(metadata.len());
            }
        }
        Ok(())
    }
    let mut total = 0;
    walk(path, &mut total).ok().map(|()| total)
}

/// Wall-clock epoch seconds, for GC grace math. Degrades to 0 (a pre-1970 clock
/// makes every stamped row look in-grace -> nothing reaped, the safe direction).
pub(crate) fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Node id carried by an automatic target/reconcile worker name.
///
/// Dispatch names are the durable join available even when a worker wedges
/// before taking its node claim. Keep the parser narrow: ad-hoc agents that
/// merely start with `target-` must never create a backlog failure.
pub(crate) fn dispatch_node_id(name: &str) -> Option<String> {
    let mut parts = name.split('-');
    match parts.next()? {
        "target" | "reconcile" => {}
        _ => return None,
    }
    let prefix = parts.next()?;
    let hex = parts.next()?;
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || hex.is_empty()
        || !hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(format!("{prefix}-{hex}"))
}

fn global_events_path(home: &AgentsHome) -> PathBuf {
    home.root()
        .parent()
        .unwrap_or_else(|| home.root())
        .join("events.jsonl")
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DispatchTermination {
    Found(String),
    Absent(Option<String>),
    Unknown(String),
}

fn dispatch_target_session_id(
    entry: &RegistryEntry,
    node_id: &str,
) -> Result<Option<String>, String> {
    let manifest = PathBuf::from(&entry.cwd).join(".fno/target-state.md");
    let content = match std::fs::read_to_string(&manifest) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("read {}: {err}", manifest.display())),
    };
    let parsed = crate::loop_target::parse_target_manifest(&content)
        .ok_or_else(|| format!("parse target session from {}", manifest.display()))?;
    if parsed.input != node_id {
        return Err(format!(
            "target manifest {} belongs to input {}, not registry node {node_id}",
            manifest.display(),
            parsed.input
        ));
    }
    if let (Some(row_session), Some(manifest_session)) = (
        entry.harness_session_id.as_deref(),
        parsed.harness_session_id.as_deref(),
    ) {
        if row_session != manifest_session {
            return Err(format!(
                "target manifest {} harness session {manifest_session} does not match registry {row_session}",
                manifest.display()
            ));
        }
    }
    Ok(Some(parsed.session_id))
}

pub(crate) fn dispatch_termination(
    home: &AgentsHome,
    entry: &RegistryEntry,
    node_id: &str,
) -> DispatchTermination {
    let session_id = match dispatch_target_session_id(entry, node_id) {
        Ok(session_id) => session_id,
        Err(err) => return DispatchTermination::Unknown(err),
    };
    let Some(session_id) = session_id else {
        // A worker that wedged before target init has no manifest and therefore
        // cannot have emitted a target-loop termination.
        return DispatchTermination::Absent(None);
    };
    let journal = crate::loop_runtime::Journal::new(
        crate::loop_runtime::ProjectJournalPath::from_caller_root(Path::new(&entry.cwd)),
        crate::loop_runtime::GlobalJournalPath(global_events_path(home)),
    );
    match journal.find_termination_strict(&session_id) {
        Ok(Some(_)) => DispatchTermination::Found(session_id),
        Ok(None) => DispatchTermination::Absent(Some(session_id)),
        Err(err) => DispatchTermination::Unknown(err.to_string()),
    }
}

pub(crate) fn record_dead_dispatch(
    home: &AgentsHome,
    entry: &RegistryEntry,
    node_id: &str,
    target_session_id: Option<&str>,
) -> Result<(), String> {
    // This global stream is the failure-streak authority. Python consumes the
    // agents-home parent even when config.state_dir differs, so a successful
    // write is durable and visible; a failed write restores the row for retry.
    EventEmitter::new(global_events_path(home), "daemon")
        .emit(
            "node_failed",
            &json!({
                "unit_id": node_id,
                "session_id": target_session_id.unwrap_or(&entry.short_id),
                "iteration": 0,
                "exit_code": 1,
                "short_id": entry.short_id,
                "reason": "agent-row-reaped-no-termination",
            }),
        )
        .map_err(|err| err.to_string())
}

pub(crate) fn restore_unaccounted_row(
    home: &AgentsHome,
    entry: &RegistryEntry,
) -> Result<(), String> {
    let mut restored = false;
    state::update_registry(&home.registry_json(), |registry| {
        if !registry.entries.iter().any(|row| row.name == entry.name) {
            registry.entries.push(entry.clone());
            restored = true;
        }
    })
    .map_err(|err| err.to_string())?;
    if restored {
        Ok(())
    } else {
        Err(format!(
            "could not restore {}: a replacement row now owns that name",
            entry.name
        ))
    }
}

/// The one POSITIVE death proof the sweep holds itself: a recorded pid whose
/// start time no longer matches provably ended. Folded into the answer type
/// so the reapers read one vocabulary; the ladder never answers `Dead` from
/// absence.
pub(crate) fn fold_positive_death(
    e: &state::RegistryEntry,
) -> Option<crate::client_verbs::RowLiveness> {
    e.pid
        .map(|p| !pid_is_ours(p, e.pid_start_time))
        .unwrap_or(false)
        .then_some(crate::client_verbs::RowLiveness::Dead)
}

/// The claude-uuid candidate handles for the sweep's ONE truth batch: the
/// ladder never launches a serial per-row `fno agents truth` subprocess
/// inside the sweep - N rows would otherwise hold the GC worker for roughly
/// N probe timeouts. Every row qualifies, not only stamped ones: the
/// ladder's `is_live` vote (x-91f3) reads the truth rung for unstamped rows
/// too, and a stamped-only batch leaves the transcript - the one marker a
/// pid-less, unstamped claude row can carry - permanently silent for that
/// vote. An empty candidate set spends nothing.
pub(crate) fn row_truth_handles(entries: &[state::RegistryEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| {
            e.claude_session_uuid
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(String::from)
        })
        .collect()
}

/// The batch over [`row_truth_handles`] as the reconcile sweep runs it,
/// returning the FULL probes, not a lowered state string: one batch feeds
/// both the liveness ladder and the title detector, and a second subprocess
/// for titles would be the same cold start paid twice per sweep.
pub(crate) fn batched_row_probes(
    entries: &[state::RegistryEntry],
    truth_tail_probes: &dyn Fn(
        &[String],
    )
        -> std::collections::HashMap<String, crate::truth_probe::TruthProbe>,
) -> std::collections::HashMap<String, crate::truth_probe::TruthProbe> {
    let handles = row_truth_handles(entries);
    if handles.is_empty() {
        return std::collections::HashMap::new();
    }
    truth_tail_probes(&handles)
}

/// The title diff the sweep's `agent_renamed` emits are built from:
/// one entry per row whose last-seen `harness_title` differs from the batch's
/// reading. The tuple is `(name, harness_session_id, from, to)` - the event
/// payload's shape, with `from` `None` on first observation. Rows without a
/// harness session id are skipped: the event names identity, and an
/// identity-less rename has no addressee. The row's
/// `name` is never written from any of this: the label is fno's, the title
/// is the harness's.
pub(crate) fn title_changes(
    entries: &[state::RegistryEntry],
    titles: &std::collections::HashMap<String, Option<String>>,
) -> Vec<(String, Option<String>, Option<String>, String)> {
    entries
        .iter()
        .filter_map(|e| {
            let uuid = e.claude_session_uuid.as_deref()?;
            let sid = e.harness_session_id.clone().filter(|s| !s.is_empty())?;
            let new_title = titles.get(uuid)?.clone()?;
            let from = e.harness_title.clone();
            if from.as_deref() == Some(new_title.as_str()) {
                return None;
            }
            Some((e.name.clone(), Some(sid), from, new_title))
        })
        .collect()
}

/// Apply the batch's title readings to the registry under the
/// caller's lock. Keyed by identity read off the snapshot
/// the batch planned from, so a row replaced under the same label between
/// snapshot and locked write cannot receive the first row's title. The
/// stored value is the DIFF BASELINE the next sweep compares against; every
/// reader is served the probe's fresh reading with this as fallback.
pub(crate) fn apply_title_changes(
    r: &mut state::Registry,
    entries: &[state::RegistryEntry],
    titles: &std::collections::HashMap<String, Option<String>>,
) {
    for (uuid, new_title) in titles {
        let Some(new_title) = new_title else {
            continue;
        };
        let Some(e0) = entries
            .iter()
            .find(|e| e.claude_session_uuid.as_deref() == Some(uuid.as_str()))
        else {
            continue;
        };
        let (harness, sid) = state::registry_write_key(e0);
        let keyed = sid
            .as_deref()
            .and_then(|sid| r.find_by_session_mut(&harness, sid));
        let target = match keyed {
            Some(e) => Some(e),
            None => r.find_mut(&e0.name),
        };
        if let Some(e) = target {
            e.harness_title = Some(new_title.clone());
        }
    }
}

/// The shared liveness ladder as production runs it (x-5d96): the reader
/// extracted from `claude_resume_argv_with_truth`, now called by the reaper
/// instead of a per-caller derivation. The sessions-dir index and the truth
/// answers are both computed ONCE per closure (one sweep), however many rows
/// probe - the truth rung reads the batched map, never a per-row subprocess.
/// Injected like `store_matches` so a sweep-level test never depends on what
/// lives in the developer's real `~/.claude`.
pub(crate) fn live_liveness_prober(
    truth: std::collections::HashMap<String, String>,
) -> impl Fn(&state::RegistryEntry) -> crate::client_verbs::RowLiveness {
    let home = crate::claude_ask::ClaudeHome::from_env();
    let index: std::cell::RefCell<Option<std::collections::HashMap<String, String>>> =
        std::cell::RefCell::new(None);
    let codex: std::cell::RefCell<Option<Option<Vec<(String, u64)>>>> =
        std::cell::RefCell::new(None);
    move |e: &state::RegistryEntry| {
        if let Some(dead) = fold_positive_death(e) {
            return dead;
        }
        let mut built = index.borrow_mut();
        if built.is_none() {
            *built = Some(crate::client_verbs::sessions_socket_index(&home));
        }
        let mut codex_built = codex.borrow_mut();
        if codex_built.is_none() {
            // ONE store walk per closure (one sweep), however many codex rows
            // probe - the same once-per-sweep shape the socket index above
            // keeps. `None` reads as the rung going silent (fail closed).
            *codex_built = Some(crate::client_verbs::codex_rollout_index(None));
        }
        crate::client_verbs::row_liveness_with_indexed(
            e,
            built.as_ref().expect("just built"),
            codex_built.as_ref().and_then(|c| c.as_deref()),
            |uuid: &str| truth.get(uuid).cloned(),
        )
    }
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
/// Run `claude stop <short>`, refusing to wait past `timeout`. `kill_on_drop`:
/// on timeout the `output()` future is dropped, so the hung child does not
/// keep running past the deadline this call gave up at (self-review finding:
/// this was hand-duplicated at the RPC call site below; one shared helper
/// now backs both).
async fn bounded_claude_stop(
    short: &str,
    timeout: Duration,
) -> Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    let stop = tokio::process::Command::new("claude")
        .arg("stop")
        .arg(short)
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(timeout, stop).await
}

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
                // sweep. A timeout leaves the marker for the next tick, since
                // it is retried every tick, which is the failure this feature
                // exists to prevent.
                let stopped = bounded_claude_stop(&short, Duration::from_secs(15)).await;
                match stopped {
                    // retired-ok: a daemon log line naming its own teardown call.
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
                        // retired-ok: a daemon log line naming its own teardown call.
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
    // An out-of-range pid is not merely absurd, it is dangerous: `pid_t` is
    // signed, so a u32 above i32::MAX wraps negative, and 4294967295 becomes -1 --
    // the "every process the caller may signal" broadcast target. `kill(-1, 0)`
    // then succeeds, `process_start_time` finds nothing, and the match below falls
    // to the trust-existence arm, so the probe returns TRUE and a caller goes on
    // to broadcast SIGTERM. Reject anything outside a real pid's range here, in
    // the shared probe, so every signalling caller inherits the guard.
    if pid <= 1 || pid > i32::MAX as u32 {
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

/// The idle-exit predicate: is any WORKER live on this home? A registry row is
/// not a reason to stay resident -- rows outlive their workers by design (the
/// GC reaps them a grace window later), so the registry-emptiness test this
/// replaced made idle-exit unsatisfiable on any machine that had ever spawned
/// a worker (x-cd31: 78 daemons at once, all idle, all orphaned). The question
/// is whether a worker is LIVE, answered by the same pair `gc_sweep_impl`
/// uses: a live worker socket, or a pid that is still ours.
///
/// A MISSING registry answers `true`: a fresh machine has never tracked a
/// worker, and lazy-exit must hold there (the documented contract covers the
/// very first daemon). An EXISTING but unreadable registry answers `false`
/// (stay resident): that is an absence with two explanations, and exiting on a
/// transient read failure would trade a moment of caution for a fleet of dead
/// workers' supervisors.
///
/// A worker socket counts as live only if something ANSWERS on it, not if the
/// file exists: a worker killed by anything that did not reap its socket (the
/// confirmed-stop path is the only reaper) leaves a stale file behind, and on a
/// pid-less live row the GC cannot settle it - file-existence liveness would
/// then pin the daemon forever, one stale socket per home reinstating the
/// never-exits defect this function exists to close. `worker_socket_reachable`
/// is the same connect probe the stop path treats as the authoritative
/// PID-reuse-immune signal.
fn no_live_worker(home: &AgentsHome) -> bool {
    let path = home.registry_json();
    if !path.exists() {
        return true;
    }
    let socket_candidates = home.scan_worker_sockets();
    let socket_is_live = |short_id: &str| {
        socket_candidates.iter().any(|s| s == short_id)
            && std::os::unix::net::UnixStream::connect(home.worker_sock(short_id)).is_ok()
    };
    state::load_registry(&path)
        .map(|r| {
            !r.entries.iter().any(|e| {
                socket_is_live(&e.short_id)
                    || e.pid
                        .map(|p| pid_is_ours(p, e.pid_start_time))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Socket bind + perms + lazy-start race.
// ---------------------------------------------------------------------------

/// How many times [`bind_supervisor_socket`] tries for the singleton lock
/// before concluding an incumbent owns it, and how long it waits between
/// tries. The product only has to exceed the microseconds a client's probe
/// holds the lock, never the lifetime of a real incumbent.
const LOCK_ACQUIRE_ATTEMPTS: usize = 6;
const LOCK_ACQUIRE_RETRY: Duration = Duration::from_millis(25);

/// How long the previous-build probe waits for an answer before treating the
/// socket as unserved. Short on purpose: it runs while we hold the exclusive
/// lock, so every millisecond here is a millisecond every client waits.
const PREVIOUS_BUILD_PROBE: Duration = Duration::from_millis(250);

/// Bind the supervisor socket, resolving the lazy-start race and stale sockets.
///
/// - Acquires an exclusive `flock` on a sidecar lockfile BEFORE touching the
///   socket at all. `try_lock()` is a positive marker (held or not), unlike a
///   connect probe that reads "absent" for a daemon merely too busy to accept
///   in time -- the failure mode that let every failed probe add a new
///   supervisor instead of replacing the incumbent (x-ef7f). On contention it
///   retries briefly (see [`LOCK_ACQUIRE_ATTEMPTS`]), because a client's
///   liveness probe takes the same lock for microseconds and conceding to that
///   would let a read-only question kill a cold start. Only a holder that
///   outlasts every retry yields [`DaemonError::AlreadyRunning`].
/// - Once the lock is held, no LOCK-AWARE process can be mid-bind, so any
///   existing socket file is stale to every same-build racer. One case escapes
///   that: a daemon from a PREVIOUS build holds the socket and knows nothing
///   about the lockfile, so a bounded connect probe runs before the unlink and
///   defers to a listener that answers.
/// - Enforce dir 0700 / socket 0600 regardless of umask, fstat-verifying after
///   (finding #6 Critical).
///
/// Returns the lock `File` alongside the listener: the caller must keep it
/// alive for the whole process lifetime (dropping it, or process exit,
/// releases the lock).
pub async fn bind_supervisor_socket(
    home: &AgentsHome,
) -> Result<(UnixListener, std::fs::File), DaemonError> {
    home.ensure_root()?;
    flock_self_test(home)?;

    let lock_path = home.supervisor_lock();
    let mut lock_file = std::fs::OpenOptions::new()
        .create(true)
        // Truncation happens below, AFTER the exclusive lock is held (the
        // content write is part of taking ownership; truncating a file another
        // holder's readers may be mid-read on would race). `create` without a
        // truncate decision is a clippy lint, so it is decided here either way.
        .truncate(false)
        .write(true)
        .open(&lock_path)?;

    // Retry rather than concede on the first `WouldBlock`. A client's liveness
    // probe takes this same lock for microseconds to ask whether anyone owns
    // the singleton, and conceding to that would let a READ-ONLY question kill
    // a legitimate cold start -- the client then reports a daemon that exited
    // during startup while no daemon runs at all. The retry separates the two
    // cases on the one axis that distinguishes them, duration: an incumbent
    // holds the lock for its entire life and still wins every retry, while a
    // probe is long gone before the second attempt.
    let mut lock_held = false;
    for attempt in 0..LOCK_ACQUIRE_ATTEMPTS {
        match lock_file.try_lock() {
            Ok(()) => {
                lock_held = true;
                break;
            }
            Err(e) => {
                let io_err: std::io::Error = e.into();
                if io_err.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(io_err.into());
                }
                if attempt + 1 < LOCK_ACQUIRE_ATTEMPTS {
                    // Back off progressively rather than at a fixed interval.
                    // A waiting client re-probes this lock every 25ms, so a
                    // fixed 25ms retry can beat against that cadence and lose
                    // every attempt to read-only probes -- a cold start would
                    // then exit claiming an incumbent that does not exist.
                    tokio::time::sleep(LOCK_ACQUIRE_RETRY * (attempt as u32 + 1)).await;
                }
            }
        }
    }
    if !lock_held {
        return Err(DaemonError::AlreadyRunning(home.supervisor_sock()));
    }

    // Record the holder in the lockfile CONTENT (x-3498): pid plus start time,
    // so `restart --force` has a SIGKILL target that survives "which daemon
    // owns this". The start time is written alongside because a bare pid is a
    // reuse hazard -- `pid_is_ours` guards the eventual signal with it. A write
    // failure is non-fatal: the flock, not this content, is the authority on
    // whether a holder exists; the content only names one.
    {
        use std::io::Write;
        let pid = std::process::id();
        let start = process_start_time(pid)
            .map(|t| t.to_string())
            .unwrap_or_default();
        if lock_file
            .set_len(0)
            .and_then(|()| lock_file.write_all(format!("{pid} {start}\n").as_bytes()))
            .and_then(|()| lock_file.flush())
            .is_err()
        {
            let _ = lock_file.set_len(0);
        }
    }

    let sock = home.supervisor_sock();
    // Holding the lock rules out a same-build competitor, so any file here is
    // stale -- with one exception the lock cannot see. A daemon from a PREVIOUS
    // build holds this socket and knows nothing about the lockfile, so the lock
    // reads free while a live listener serves. Unlinking there recreates the
    // exact defect this guard removes, once at every `fno doctor update`.
    //
    // So keep a connect probe as a BELT on top of the lock, never instead of
    // it. It is only ever reached when no lock-aware daemon holds the
    // singleton, which is precisely the upgrade case: every same-build race is
    // already decided above, where a busy incumbent that would fail this probe
    // still holds the lock and this line is never reached.
    //
    // BOUNDED, and that bound is load-bearing. A blocking connect here is this
    // whole defect in miniature: an old-build listener whose backlog is full
    // leaves connect() hanging, and we hold the exclusive lock while it hangs,
    // so every client verb reports a busy incumbent forever. A listener that
    // cannot answer in 250ms is serving nobody, so a timeout falls through to
    // the unlink. Deferring to it instead would leave the machine with no
    // reachable daemon at all, which is strictly worse than displacing a
    // process that is already unreachable.
    let previous_build_serving =
        tokio::time::timeout(PREVIOUS_BUILD_PROBE, tokio::net::UnixStream::connect(&sock))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
    if previous_build_serving {
        return Err(DaemonError::AlreadyRunning(sock));
    }
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Defensive: should be unreachable while we hold the lock.
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

    Ok((listener, lock_file))
}

/// True when `sock` still resolves to the inode we originally bound. A
/// mismatch means something else unlinked and rebound the path out from under
/// us (an operator `rm`, or a bug elsewhere) -- we no longer own the
/// reachable path (x-ef7f / x-e98b).
///
/// Sound only because the caller is still LISTENING on that inode. An inode
/// number is free to be recycled once nothing references it, and Linux
/// recycles eagerly, so comparing numbers would be worthless for a path we had
/// let go. Our open listener pins ours for the daemon's whole life, so a
/// replacement file at the same path necessarily gets a different number.
fn socket_inode_matches(sock: &Path, bound_ino: u64) -> bool {
    std::fs::metadata(sock)
        .map(|m| m.ino() == bound_ino)
        .unwrap_or(false)
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

    // Startup row-count assertion (x-4c87 AC5), BEFORE the socket is bound: a
    // registry with rows on disk that the typed reader cannot fully decode must
    // refuse startup here -- exit nonzero, stderr naming the path and both
    // counts -- rather than bind, serve, and answer every caller from a
    // silently emptied roster (the false "0 registered agents" outage: a stale
    // daemon swallowing its own read failure while discovery kept answering).
    load_registry_asserted(&home.registry_json())?;

    // State: cold_start.
    // `_supervisor_lock` is a named (not `let _`) binding: it must stay alive
    // for the rest of this function so the flock is held for the daemon's
    // whole lifetime. Only the leading underscore (suppressing the "unused"
    // lint) matters -- nothing ever reads the `File` again.
    let (listener, _supervisor_lock) = match bind_supervisor_socket(&home).await {
        Ok(pair) => pair,
        Err(DaemonError::AlreadyRunning(_)) => {
            // Race loser: nothing to do; the winner serves.
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let sock_path = home.supervisor_sock();
    let bound_ino = std::fs::metadata(&sock_path).ok().map(|m| m.ino());

    // State: recovering. Recovery must complete before we accept a request.
    emit_state(&emitter, DaemonState::Recovering);
    let destructive = crate::agents_config::startup_destructive_recovery_enabled(
        &std::env::current_dir().unwrap_or_else(|_| home.root().to_path_buf()),
    );
    let report = recover_with_policy(&home, &emitter, destructive)?;

    // Architecture B (plan ab-70faa65b): ONE bounded reconcile sweep on startup,
    // as part of recovery, CONCURRENTLY with the accept loop (x-ef7f), so a
    // large roster no longer keeps a cold daemon silent while it probes. Reuses
    // the same bounded machinery as the `reconcile` RPC (fairness order +
    // 250ms/probe + 5s budget). Strictly non-fatal: a sweep that returns an
    // error (registry write failed -> registry unchanged) or even panics
    // degrades to serving last-recorded rows -- we emit and continue, never
    // abort the daemon (AC1-FR). No client observes a half-applied sweep, which
    // the registry's advisory lock gives rather than the old ordering: what the
    // ordering additionally gave, and this does not, is a guarantee that the
    // FIRST `list` is post-sweep. Opt out via FNO_AGENTS_NO_STARTUP_RECONCILE
    // for the fastest cold start (discretion #5).
    if opts.reconcile_on_start {
        // Off the startup path and onto the blocking pool (x-ef7f). The sweep
        // probes reachability PER REGISTRY ROW, each probe bounded but not
        // free, so on a large roster it costs tens of seconds. Awaiting it here
        // -- on the async runtime, before the accept loop starts -- meant a
        // cold daemon answered nothing until every row was probed, and a client
        // whose connect timed out lazy-started yet another daemon that paid the
        // same cost. One cold daemon starved itself; the spawns compounded it.
        //
        // What that ordering bought was "no client observes a half-applied
        // sweep", and that guarantee is not what is given up here: the sweep
        // writes the registry under the same advisory lock every reader takes,
        // so a client still reads a whole registry, never a torn one. What is
        // given up is narrower -- the FIRST `list` after a cold start may read
        // pre-sweep rows, one idle tick before the sweep lands. A stale first
        // row is worth strictly less than a daemon nobody can reach.
        let home_sweep = home.clone();
        let emitter_sweep = EventEmitter::new(home.events_jsonl(), "daemon");
        tokio::task::spawn_blocking(move || {
            // Test seam (x-ef7f): hold the sweep open so a test can prove the
            // daemon answers DURING it, not merely after it. Without a seam that
            // assertion is a race against however fast the machine probes, and a
            // flaky proof of the one property this fix exists to give. Never set
            // in production, exactly like FNO_AGENTS_FAIL_STARTUP_RECONCILE below.
            if let Some(ms) = std::env::var("FNO_AGENTS_STARTUP_RECONCILE_DELAY_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
            {
                std::thread::sleep(Duration::from_millis(ms));
            }
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
                        // Registry-side keeper sweep FIRST (x-ac6b): re-bind
                        // surviving lane-B thread rows BEFORE the settle pass
                        // below reads them - the daemon-side twin of the pane
                        // sweep's re-adopt-before-restore ordering. Non-fatal
                        // on error: emit and serve past, like the sweep below.
                        match keeper_registry_sweep(&home_sweep, &emitter_sweep) {
                            Ok(report) => {
                                // Store-socket hygiene rides the same startup
                                // pass: dead store sockets unlinked, live
                                // ones untouched. Non-fatal by posture.
                                let store_unlinked =
                                    store_socket_sweep(&home_sweep, &emitter_sweep);
                                let _ = emitter_sweep.emit(
                                    "keeper_sweep_done",
                                    &json!({
                                        "sockets": report.sockets,
                                        "rebound": report.rebound.len(),
                                        "dead": report.dead.len(),
                                        "wedged": report.wedged.len(),
                                        "superseded": report.superseded.len(),
                                        "store_unlinked": store_unlinked,
                                    }),
                                );
                            }
                            Err(msg) => {
                                let _ = emitter_sweep
                                    .emit("keeper_sweep_failed", &json!({"error": msg}));
                            }
                        }
                        // Startup sweep: every thread row reads hosted. The
                        // async recovery pass owns resume-and-settle here and
                        // has not run yet, so settling unhosted rows now would
                        // stamp Orphaned rows the recovery pass is about to
                        // resume.
                        run_reconcile_sweep(&home_sweep, &emitter_sweep, &|_| true)
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
                    let _ = emitter_sweep.emit(
                        "startup_reconcile_done",
                        &json!({
                            "updated": result.outcome.updated.len(),
                            "deferred": result.outcome.deferred,
                        }),
                    );
                }
                Err(msg) => {
                    let _ = emitter_sweep.emit("startup_reconcile_failed", &json!({"error": msg}));
                }
            }
        });
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
            "recovery_mode": report.recovery_mode,
            "interrupted_write_temps": report.interrupted_write_temps.len(),
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
        codex_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    });
    schedule_codex_thread_recovery(Arc::clone(&ctx));

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
        let fno_bin = std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string());
        let ab_emitter = EventEmitter::new(ctx.home.events_jsonl(), "active-backlog");
        let live = Arc::clone(&ab_live);
        let shutdown = Arc::clone(&ab_shutdown);
        tokio::spawn(crate::active_backlog::run_supervisor(
            fno_bin, ab_emitter, live, shutdown,
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
    let worktree_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Orphaned-test-binary reap gate: same one-in-flight discipline. The verb
    // it shells to runs ps + a kill, so it never runs on the core loop.
    let orphan_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut last_orphan_sweep = Instant::now();
    // Dead-row GC gate (x-ef7f): its dormant check shells out to the truth
    // probe, so it gets the same one-in-flight discipline as the sweeps beside
    // it rather than running inline in the select arm.
    let gc_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Idle-exit liveness probe gate + verdict handoff: the probe is blocking
    // I/O (a connect per socket candidate), so the arm spawns it and reads
    // the completed verdict on a later tick. A verdict is only ever consumed
    // when it is still FRESH: the request activity it ran under is unchanged
    // (a served request resets the idle clock) AND the registry it read has
    // not been written since (a pane-substrate worker spawns by writing the
    // registry directly, with no daemon contact - the mtime is the one
    // positive marker of that). Anything stale is discarded unread.
    let idle_probe_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Stale-question reconcile: same one-in-flight discipline as the sweeps
    // beside it. The verb dedupes on outcome identity, so an extra run is a
    // no-op; the gate exists so a slow fleet probe never stacks.
    let stale_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let idle_probe_verdict: Arc<
        std::sync::Mutex<Option<(bool, Instant, Option<std::time::SystemTime>)>>,
    > = Arc::new(std::sync::Mutex::new(None));

    // THE RULE FOR THIS LOOP (x-ef7f): nothing that shells out, walks the
    // registry row by row, or otherwise blocks may run INLINE in a select arm.
    // Every arm shares one thread with `accept()` and with the SIGTERM arm, so
    // an inline sweep makes the daemon both unreachable and unstoppable at the
    // same time -- which is how 59 supervisors accumulated, each new client
    // reading the silence as "no daemon" and starting another. Give a sweep
    // `spawn_blocking` plus a one-in-flight `AtomicBool`, like the four below.
    // The reason the serve loop ended, threaded to the shared exit tail so
    // `daemon_exited` can tell an abnormal ending from a graceful one
    // (x-3498): every break carries its reason string.
    let exit_reason: &str = loop {
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
                break "sigterm";
            }
            _ = idle_check.tick() => {
                // Bound-inode self-check (x-ef7f): if the socket path no
                // longer resolves to the inode we bound, something else now
                // owns it (an operator `rm`, or a bug elsewhere) -- retire
                // rather than keep serving unreachable forever.
                if let Some(ino) = bound_ino {
                    if !socket_inode_matches(&sock_path, ino) {
                        let _ = ctx.emitter.emit(
                            "daemon_socket_lost",
                            &json!({"reason": "socket path no longer resolves to our bound inode"}),
                        );
                        emit_state(&ctx.emitter, DaemonState::ShuttingDown);
                        let _ = ctx.emitter.emit(
                            "daemon_shutting_down",
                            &json!({"reason": "socket-lost"}),
                        );
                        break "socket-lost";
                    }
                }
                // Reap any worker that exited since the last tick so it never
                // lingers as a zombie under the long-lived daemon.
                crate::orphan_reap::reap_daemon_children();
                // Screen-manifest scrape sweep (the badge-lattice fallback
                // rung): subprocesses + file IO, so it runs off-loop under
                // spawn_blocking behind the one-in-flight gate.
                if !scrape_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&scrape_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    let notify_on_blocked = ctx.opts.notify_on_blocked;
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        crate::scrape::scrape_sweep(&home, &emitter, notify_on_blocked);
                    });
                }
                // Retirement sweep (x-c672): a row leaves when its WORK is
                // done (reverse join) and its transcript is quiet past
                // `agents.retire_grace_s`; its held process is stopped first,
                // its receipt is written before the drop, and its
                // clean-and-merged worktree is pruned. Cheap in steady state
                // (no candidates -> no probes, no registry write).
                // Runs off-loop under spawn_blocking behind a one-in-flight
                // gate, like the scrape and worktree sweeps above (x-ef7f):
                // inline, a slow sweep starved accept() and the SIGTERM arm
                // beside it.
                if !gc_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&gc_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    let grace_cwd = ctx.opts.agents_config_cwd.clone();
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        let grace_secs =
                            crate::agents_config::retire_grace_secs(&grace_cwd) as i64;
                        let retain_days =
                            crate::agents_config::reap_receipt_retain_days(&grace_cwd);
                        let _ = gc_sweep(&home, &emitter, grace_secs, retain_days);
                        crate::gc::unowned_sweeps(&home, &emitter, &grace_cwd);
                    });
                }
                // Worktree sweep: the backstop for what the merge ritual
                // missed. Its own 24h stamp makes it a near-no-op on this tick,
                // but the verb shells git across every worktree when it does
                // fire, so it runs off-loop behind a one-in-flight gate like the
                // scrape sweep. Report-only unless a merge-minted reap order
                // (reap:pr-* claim) stands, in which case the pass applies.
                if !worktree_sweep_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&worktree_sweep_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        let roots = registry_repo_roots(&home);
                        let now = now_epoch_secs();
                        worktree_sweep(&home, &emitter, now, &roots, &|root| {
                            if merge_cleanup_requested(&home, root) {
                                return WorktreeSweepOrderRead::from(true);
                            }
                            // Compatibility with requests minted by older
                            // rituals: a standing claim still authorizes the
                            // guarded report/apply sweep during the deploy window.
                            // Live reap orders anywhere (both claim roots are
                            // read by `list`): each is minted only by a ritual
                            // that gh-confirmed MERGED, so its standing is the
                            // merge-trigger for this tick's apply pass.
                            (!crate::claims::list(
                                Some("reap:"),
                                Some(std::path::Path::new(root)),
                                false,
                            )
                            .is_empty())
                                .into()
                        }, &|root, apply| {
                            let mut cmd = std::process::Command::new("fno");
                            cmd.current_dir(root)
                                .env("FNO_AGENTS_HOME", home.root())
                                .args([
                                "agents", "workspace", "worktree", "cleanup", "--merged",
                                ]);
                            if apply {
                                cmd.arg("--apply");
                            }
                            match cmd.output() {
                                Ok(output) => WorktreeSweepOutput {
                                    exit_code: output.status.code(),
                                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                                },
                                Err(error) => WorktreeSweepOutput {
                                    exit_code: None,
                                    stdout: String::new(),
                                    stderr: error.to_string(),
                                },
                            }
                        });
                        consume_merge_cleanup_requests(&home, &roots, &emitter);
                    });
                }
                // Orphaned-test-binary reap: the waitpid sweep above only ever
                // sees the daemon's OWN children; a wedged deps/ test binary at
                // ppid 1 holding zombie corpses is invisible to waitpid(-1), and
                // this arm is what reaches that shape. The whole arm - cadence,
                // gate, kill, events - lives in crate::orphan_reap.
                crate::orphan_reap::maybe_sweep(
                    &mut last_orphan_sweep,
                    &orphan_sweep_in_flight,
                    ctx.home.events_jsonl(),
                );
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
                        let _gate = SweepGate(flag);
                        terminal_stop_sweep(&home, &emitter).await;
                    });
                }
                // Stale-question reconcile: rows past the wake ceiling are the
                // watchdog's needs-human bucket, and no lane acts on them, so
                // the durable question channel is the only surface they reach.
                // This arm is that channel's trigger. Its own 6h stamp bounds
                // discovery lag; the verb inside dedupes on outcome identity,
                // so an eager run costs one sweep and asks nothing. The verb
                // never gets an apply form: this lane routes information and
                // changes no removal path.
                if !stale_sweep_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&stale_sweep_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        stale_sweep(&home, &emitter, now_epoch_secs(), &|| {
                            std::process::Command::new("fno")
                                .args(["agents", "stale-escalate", "--json"])
                                .output()
                                .ok()
                                .filter(|o| o.status.success())
                                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        });
                    });
                }
                // An enabled active-backlog project keeps the daemon resident even
                // when the board is drained (OQ1 Option A): idle-exit must never
                // kill a live drain supervisor.
                let ab_active = ab_live.load(std::sync::atomic::Ordering::SeqCst);
                if !ab_active && last_activity.elapsed() >= ctx.opts.idle_exit {
                    // The liveness read (a CONNECT probe per socket candidate)
                    // is blocking I/O, so it runs OFF the select arm like the
                    // sweeps above, never inline: an in-arm probe against a
                    // wedged worker's filling backlog is the
                    // unreachable-AND-unstoppable shape this loop's rule
                    // exists to prevent. One probe in flight; the exit fires
                    // on the tick that reads a completed no-live-worker
                    // verdict, so the worst case is one extra 5s tick.
                    if !idle_probe_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        let flag = Arc::clone(&idle_probe_in_flight);
                        let home = ctx.home.clone();
                        let verdict = Arc::clone(&idle_probe_verdict);
                        let probe_activity = last_activity;
                        tokio::task::spawn_blocking(move || {
                            let _gate = SweepGate(flag);
                            let no_worker = no_live_worker(&home);
                            // mtime AFTER the reads: a registry write that
                            // raced the probe is caught by the change.
                            let mtime = std::fs::metadata(home.registry_json())
                                .ok()
                                .and_then(|m| m.modified().ok());
                            *verdict.lock().unwrap() = Some((no_worker, probe_activity, mtime));
                        });
                    }
                    let verdict = idle_probe_verdict.lock().unwrap().take();
                    let fresh = verdict.is_some_and(|(no_worker, probe_activity, probe_mtime)| {
                        let mtime_now = std::fs::metadata(ctx.home.registry_json())
                            .ok()
                            .and_then(|m| m.modified().ok());
                        no_worker
                            && probe_activity == last_activity
                            && mtime_now == probe_mtime
                    });
                    if fresh {
                        emit_state(&ctx.emitter, DaemonState::IdlePendingExit);
                        let _ = ctx.emitter.emit("daemon_idle_pending_exit", &json!({}));
                        emit_state(&ctx.emitter, DaemonState::ShuttingDown);
                        let _ = ctx.emitter.emit(
                            "daemon_shutting_down",
                            &json!({"reason": "idle"}),
                        );
                        break "idle";
                    }
                }
            }
        }
    };

    // Wind down the active-backlog supervisor: signal it to stop scheduling new
    // ticks, then abort its await. An in-flight tick's spawn_blocking thread is
    // not abortable, but that is safe by design - the dispatched worker owns its
    // node:<id> claim independently, and on the next daemon start the live-claims
    // filter excludes the still-in-flight node (no double-dispatch).
    ab_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    ab_handle.abort();

    // Only reap the socket if it's still ours -- never unlink a live
    // successor's socket (x-e98b), the same discipline stop_worker_confirmed
    // already applies to worker sockets ("never unlink a live worker's
    // socket"). No captured inode (bind-time metadata read failed) falls back
    // to today's unconditional behavior.
    let still_ours = bound_ino
        .map(|ino| socket_inode_matches(&sock_path, ino))
        .unwrap_or(true);
    if still_ours {
        let _ = std::fs::remove_file(&sock_path);
    }
    emit_state(&ctx.emitter, DaemonState::Exited);
    let _ = ctx
        .emitter
        .emit("daemon_exited", &daemon_exited_payload(exit_reason));
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
    /// Live connections to the SHARED codex app-server daemon, one per codex
    /// thread worker, keyed by registry name. Not children: this supervisor
    /// owns no app-server process, so an entry here is a socket, and losing
    /// one loses a connection rather than a thread. The registry's full
    /// `harness_session_id` remains the durable join key used to repopulate
    /// this map after a daemon restart.
    codex_threads: Arc<tokio::sync::Mutex<std::collections::HashMap<String, CodexThreadHandle>>>,
}

/// Cap on the early-push buffer (E3.3). A report for a NEW session is dropped
/// (logged `buffer_full`) once the buffer is at cap; an already-buffered
/// session's seq still advances (no new key). 64 covers any realistic burst of
/// panes registering at once while staying a hard ceiling.
const PENDING_INSIDE_LEG_CAP: usize = 64;

/// One actor task per thread owns its daemon connection exclusively (x-de10).
/// This used to be `Arc<tokio::sync::Mutex<CodexThread>>`, which baked
/// whole-turn exclusion into the HANDLE TYPE: `drive_turn` held the guard for
/// up to `TURN_TIMEOUT` (600s), so every follow-up ask blocked behind the
/// active turn, the steer RPC was unreachable, the detached seed task held the
/// same lock, and `stop` removed a handle whose turn task still owned a clone
/// while stamping `Exited`. Consumers now send [`ThreadCommand`]s and never
/// touch the driver; see `crates/fno-agents/src/codex_thread.rs`.
type CodexThreadHandle = Arc<crate::codex_thread::CodexThreadActor>;

use crate::codex_thread::{InterruptOutcome, TurnReceipt};

/// The per-completion hook every codex-thread actor gets at construction:
/// bump the row (`Live` + `last_message_at`) and emit `agent_ask_done`, for
/// every submitter class (ask, seed, mail steer) in ONE place - previously
/// the ask path and the seed task each kept their own copy of this.
fn codex_thread_on_done(
    emitter: &EventEmitter,
    registry_path: std::path::PathBuf,
    name: &str,
) -> Arc<dyn Fn(TurnReceipt) + Send + Sync> {
    let emitter = emitter.clone();
    let name = name.to_string();
    Arc::new(move |receipt: TurnReceipt| {
        let emitter = emitter.clone();
        let name = name.clone();
        let turn_id = receipt.turn_id.clone();
        let status = receipt.status.clone();
        let registry_path = registry_path.clone();
        tokio::spawn(async move {
            let bump_name = name.clone();
            let _ = update_registry_offloaded(registry_path, move |registry| {
                if let Some(entry) = registry.find_mut(&bump_name) {
                    entry.status = AgentStatus::Live;
                    entry.last_message_at = Some(now_rfc3339_like());
                }
            })
            .await;
            let _ = emitter.emit(
                "agent_ask_done",
                &json!({
                    "name": name,
                    "backend": "codex-thread",
                    "turn_id": turn_id,
                    "turn_status": status,
                }),
            );
        });
    })
}

fn emit_state(emitter: &EventEmitter, state: DaemonState) {
    let _ = emitter.emit("daemon_state", &json!({"state": state.as_str()}));
}

/// The final `daemon_exited` payload (x-3498). Every exit path flows through
/// one tail, and before this it emitted `clean: true` unconditionally, so the
/// socket-lost retirement - where something unlinked and rebound our socket
/// path - logged identically to a graceful SIGTERM shutdown. A watchdog
/// reading `daemon_exited` alone could not tell them apart; `clean` is false
/// only for that abnormal ending, and `reason` names which path fired.
fn daemon_exited_payload(reason: &str) -> Value {
    json!({"clean": reason != "socket-lost", "reason": reason})
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

/// Clears a one-in-flight sweep gate on drop.
///
/// Every sweep below sets its gate, then runs off-loop and clears the gate as
/// the closure's last statement. A PANICKING sweep never reaches that
/// statement: the panic is captured by a `JoinHandle` the spawner immediately
/// drops, so the gate stays latched and that sweep is silently disabled for the
/// daemon's whole life. `gc_sweep` and the scrape sweep both shell out and
/// parse the output, so this is not hypothetical. A guard clears the gate on
/// the unwind path too.
pub(crate) struct SweepGate(pub(crate) Arc<std::sync::atomic::AtomicBool>);

impl Drop for SweepGate {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
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
        // Same teardown casualty as load_registry_offloaded below: a
        // queued-but-not-started handler dropped by shutdown, not a fault
        // in the handler itself.
        Err(e) if e.is_cancelled() => Response::err(
            id,
            ErrorCode::ShuttingDown,
            format!("handler task cancelled during shutdown: {e}"),
        ),
        Err(_) => Response::err(id, ErrorCode::Internal, "handler task panicked"),
    }
}

/// The daemon-face x-4c87 read: the typed decode plus the raw-count
/// assertion, on EVERY roster read the daemon serves (startup, recovery, and
/// every RPC handler). The tolerant state reader still returns a PARTIAL
/// registry for a future-schema store with announced row drops, which the
/// read-modify-write path needs; a daemon that serves that partial roster as
/// the complete roster is the false-zero outage at runtime, because the
/// startup assertion never re-runs (codex P1 on PR 924).
pub(crate) fn load_registry_asserted(
    path: &std::path::Path,
) -> Result<state::Registry, state::StateError> {
    let (registry, raw_rows) = state::load_registry_with_counts(path)?;
    if registry.entries.len() != raw_rows {
        return Err(state::StateError::InvariantViolation(
            state::registry_row_divergence_msg(path, raw_rows, registry.entries.len()),
        ));
    }
    Ok(registry)
}

/// Offload the blocking flock + file read of `load_registry_asserted` to the
/// blocking pool so it never stalls an async handler's runtime thread
/// (ab-e86e326b). Mirrors the `update_registry_offloaded` wrapper and the
/// `run_blocking` helper. Since x-4c87 a join failure maps to a `StateError`
/// (like `update_registry_offloaded`) and a read error propagates: both used
/// to collapse to the empty registry, which turned an unreadable registry into
/// the valid-looking answer "zero agents" for every caller below.
async fn load_registry_offloaded(path: PathBuf) -> Result<state::Registry, state::StateError> {
    match tokio::task::spawn_blocking(move || load_registry_asserted(&path)).await {
        Ok(result) => result,
        // A queued-but-not-yet-started blocking task is dropped, not run, when
        // the runtime shuts down (src/bin/daemon.rs:76 waits only on
        // already-started ones) -- a teardown casualty, never a fault in the
        // read itself.
        Err(e) if e.is_cancelled() => Err(state::StateError::Cancelled(format!(
            "load_registry task cancelled: {e}"
        ))),
        Err(e) => Err(state::StateError::Io(std::io::Error::other(format!(
            "load_registry task panicked: {e}"
        )))),
    }
}

/// A `StateError::Cancelled` is a teardown casualty, not a fault in the read
/// or write itself; every other variant stays the catch-all internal fault.
/// The one classification choke point both `registry_read_failed` and every
/// `update_registry_offloaded` call site route through, so a shutdown-time
/// cancellation gets `ShuttingDown` regardless of which verb hit it.
fn state_error_code(e: &state::StateError) -> ErrorCode {
    match e {
        state::StateError::Cancelled(_) => ErrorCode::ShuttingDown,
        // Both halves of the schema comparison answer the same way: the write
        // was refused because two fno builds disagree about the schema, which a
        // client must be able to tell apart from an internal daemon fault.
        state::StateError::UnsupportedSchemaVersion { .. }
        | state::StateError::SourceAheadSchemaBump { .. } => ErrorCode::SchemaMismatch,
        _ => ErrorCode::Internal,
    }
}

/// The x-4c87 RPC face of a registry-read failure: every handler that consults
/// the registered roster reports `registry read failed` carrying the state
/// error (which names the registry path, both row counts, and the comparison
/// to run) instead of answering from a silently emptied roster. `AgentNotFound`
/// stays reserved for a successful read with no matching row.
fn registry_read_failed(id: u64, e: state::StateError) -> Response {
    let code = state_error_code(&e);
    Response::err(id, code, format!("registry read failed: {e}"))
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
        // Same teardown casualty as load_registry_offloaded: a queued write
        // dropped by shutdown before it ran, never a panic in the write.
        Err(e) if e.is_cancelled() => Err(state::StateError::Cancelled(format!(
            "update_registry task cancelled: {e}"
        ))),
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
        Some("review-start") => handle_review_start(ctx, req).await,
        Some("switchboard") | Some("switchboard_v2") => handle_switchboard(ctx, req).await,
        Some("stop") => handle_stop(ctx, req).await,
        Some("rm") => handle_rm(ctx, req).await,
        Some("list") => run_blocking(ctx, req, handle_list).await,
        // The subscription verb: version-gated full document,
        // so a subscriber pays a stat per idle tick and a read per write.
        Some("watch") => run_blocking(ctx, req, handle_watch).await,
        // status reads the in-memory drive table for the active-drives count, so
        // it stays on the async runtime rather than the blocking pool.
        Some("status") => handle_status(ctx, req).await,
        Some("reconcile") => run_blocking(ctx, req, handle_reconcile).await,
        // Label rename: the registry transaction under the flock, off-loop.
        Some("rename") => run_blocking(ctx, req, handle_rename).await,
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
        Some(n) if state::is_valid_registry_label(n) => n.to_string(),
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
    // Post-G4 (x-f54c): the daemon hosts no agent PTYs, so the only spawns it
    // still serves are the claude stream-json ADOPTION lane -- host_mode=interactive
    // + mode=stream_json resumes an idle session as a held stream thread
    // (`claude -p --resume <uuid>`) for chat/switchboard/ask to drive -- and
    // attach-with-server thread spawns. Every interactive PTY host (codex,
    // gemini, claude) moved to the mux, and bg/headless never reach the daemon,
    // so any other spawn is a retired PTY-hosting request and errors with a
    // mux pointer. The thread substrate is routed from the capability contract
    // (`thread_lane` + `attach_needs_server`), never the harness name, and
    // every arm other than attach-with-server refuses, so the daemon still
    // hosts no agent PTYs.
    let host_mode = p
        .get("host_mode")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::state::HOST_MODE_EXEC);
    let resume_id = p
        .get("resume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let substrate = p
        .get("substrate")
        .and_then(|v| v.as_str())
        .unwrap_or("pane");
    if substrate == "thread" {
        return route_thread_spawn(ctx, req, &name, &cwd, &provider).await;
    }
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

/// Route a thread-substrate spawn from the capability contract alone, never
/// the harness name: `thread_lane` picks the lane, and on the attach lane
/// `attach_needs_server` splits harness-owned-server attaches (the app-server
/// lane below) from harness-owned-client ones, which this daemon cannot serve.
/// An unreadable table or unknown harness refuses rather than routing. Every
/// arm but attach-with-server refuses, so the x-f54c invariant holds.
async fn route_thread_spawn(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    cwd: &Path,
    provider: &str,
) -> Response {
    let contract = match crate::harness_capabilities::HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => {
            return thread_spawn_refusal(
                ctx,
                req,
                name,
                provider,
                &format!("thread spawn refused: capability table unreadable: {error}"),
            );
        }
    };
    match contract.thread_lane(provider) {
        Ok("attach") => match contract.attach_needs_server(provider) {
            Ok(true) => spawn_codex_thread_lane(ctx, req, name, cwd, provider).await,
            Ok(false) => thread_spawn_refusal(
                ctx,
                req,
                name,
                provider,
                &format!(
                    "thread spawn refused: harness {provider} hosts its own detached thread \
                     client, so the daemon has no thread to hold for it. Spawn it through the \
                     client-side detached lane (`fno agents spawn --substrate thread`)."
                ),
            ),
            Err(error) => thread_spawn_refusal(
                ctx,
                req,
                name,
                provider,
                &format!("thread spawn refused: {error}"),
            ),
        },
        Ok("keeper") => thread_spawn_refusal(
            ctx,
            req,
            name,
            provider,
            &format!(
                "thread spawn refused: harness {provider} is a keeper-lane harness; fno's \
                 keeper process holds the pty for its thread. Spawn it through the fno CLI's \
                 keeper entry point, not the daemon."
            ),
        ),
        Ok(_) => thread_spawn_refusal(
            ctx,
            req,
            name,
            provider,
            &format!(
                "thread spawn refused: harness {provider} declares no interactive resume \
                 form, so no thread lane exists for it"
            ),
        ),
        Err(error) => thread_spawn_refusal(
            ctx,
            req,
            name,
            provider,
            &format!("thread spawn refused: {error}"),
        ),
    }
}

fn thread_spawn_refusal(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    provider: &str,
    reason: &str,
) -> Response {
    let _ = ctx.emitter.emit(
        "agent_spawn_failed",
        &json!({
            "name": name,
            "provider": provider,
            "substrate": "thread",
            "reason": reason,
        }),
    );
    Response::err(req.id, ErrorCode::InvalidParams, reason)
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
/// The agent-list row's substitution marker (x-2019): the object naming BOTH
/// values on a substituted verdict, null on match-or-unknown. Null is the
/// unknown shape too - a row whose probe has not answered must never read as
/// clean. Mirrors `format._model_substitution_marker` in the Python emitter.
fn model_substitution_marker(
    requested: Option<&str>,
    observed: &serde_json::Value,
) -> serde_json::Value {
    if crate::state::model_substitution(requested, Some(observed)) == "substituted" {
        json!({
            "requested": requested,
            "observed": observed.get("model"),
        })
    } else {
        serde_json::Value::Null
    }
}

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
    // Ambient parent edge (x-132c), captured for shape parity with the other
    // mint sites. This fn runs IN THE DAEMON, and lazy-start scrubs the
    // harness session markers from the daemon's env (client.rs), so this
    // stamps None by construction: the daemon itself started this PTY worker
    // and no session parent is claimable from here. A daemon that somehow
    // still carries a marker attributes nothing rather than laundering it.
    let (parent_session, parent_harness, parent_cwd) = crate::claims::ambient_parent_edge();
    let (launch_account, launch_account_source) = crate::state::launch_provenance_from_env();
    RegistryEntry {
        node: None,
        // Stream-json adoption is gated on host_mode plus mode, not on a
        // substrate, and it is not one of the three names - this row's
        // lifecycle belongs to chat/switchboard/ask, so the axis stays
        // unknown rather than forcing a "thread" stamp.
        substrate: None,
        name: name.into(),
        short_id: short_id.into(),
        // Birth marker: the daemon started this PTY worker itself. An absent origin means UNKNOWN,
        // and the watchdog's retire lane never acts on unknown.
        origin: Some("spawn".to_string()),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        // v23 (x-2019): adoption - the daemon observed no spawn request, so
        // the requested axis stays unknown rather than a guess.
        requested_model: None,
        requested_provider: None,
        requested_effort: None,
        harness: Some("claude".into()),
        harness_session_id: Some(uuid.into()),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        // x-d285: the daemon env is what this claude child inherits, so the
        // three-valued env read is honest (ambient config dir = unknown).
        launch_account: launch_account.clone(),
        launch_account_source,
        related_session_id: None,
        // v25: the vendor route is unobserved on this lane (it may be routed,
        // and `provider` above stays None for the same reason), so it stays
        // unknown here rather than guessing "anthropic". The account record
        // mirrors the launch read - unknown stays unknown.
        route_provider_id: None,
        model_name: None,
        account_record_id: launch_account,
        cwd: cwd_s.clone(),
        project_root: cwd_s,
        session_id: None,
        spawn_trigger: None,
        spawned_by_session: parent_session,
        spawned_by_harness: parent_harness,
        spawned_by_cwd: parent_cwd,
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
        keeper_child_pid: None,
        log_path: Some(log_path.to_string_lossy().into_owned()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        ..Default::default()
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
    //    atomically under the registry lock at registration). A read failure is
    //    fatal to the spawn (x-4c87): an empty-roster default here would read
    //    every existing name as free.
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
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
        e.harness_name() == "claude"
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
            e.harness_name() == "claude"
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
            return Response::err(req.id, state_error_code(&e), format!("registry write: {e}"));
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
        json!({"short_id": short_id, "harness": "claude", "status": "live", "lane": "stream"}),
    )
}

/// Start and register one Codex app-server thread. The seed turn is detached
/// after registration so spawn returns a live row immediately while the held
/// process remains available for later `ask` calls. Reached by the derived
/// route in [`route_thread_spawn`]: the only attach-with-server destination
/// built, which is why it keeps its honest codex name. That name is also the
/// precondition: the body drives CodexThread, so a provider it cannot serve
/// refuses here rather than silently starting a codex thread under the
/// caller's name. The guard is the destination's own (the route stays
/// name-free), and it is what a SECOND attach-with-server row meets until
/// its destination is wired.
async fn spawn_codex_thread_lane(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    cwd: &Path,
    provider: &str,
) -> Response {
    if provider != "codex" {
        return thread_spawn_refusal(
            ctx,
            req,
            name,
            provider,
            &format!(
                "thread spawn refused: the only attach-with-server destination built drives \
                 the codex app-server; harness {provider} needs its own thread destination \
                 wired before its spawn can be served"
            ),
        );
    }
    let model = req.params.get("model").and_then(Value::as_str);
    let yolo = req
        .params
        .get("yolo")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let effort = req.params.get("effort").and_then(Value::as_str);
    let node = req.params.get("node").and_then(Value::as_str);
    // Hop 2 of the state-root grant (x-f22f). Read the roots from the REQUEST,
    // never from this process's environment. This daemon is long-lived and
    // shared across every thread on the machine, so its own env is not the
    // spawning client's - a `state_dirs_from_env()` call here would read
    // whatever shell started the daemon, which is the exact mistake the next
    // reader of this function will be tempted to make.
    let state_dirs: Vec<String> = req
        .params
        .get("state_dirs")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|dir| !dir.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let seed = req
        .params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let driver = match crate::codex_thread::CodexThread::start_with_state_dirs(
        cwd.to_path_buf(),
        model,
        yolo,
        effort,
        &state_dirs,
    )
    .await
    {
        Ok(driver) => driver,
        Err(error) => {
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "provider": "codex", "lane": "thread", "reason": error.to_string()}),
            );
            return Response::err(req.id, ErrorCode::SpawnFailed, error.to_string());
        }
    };
    let entry = build_codex_thread_entry(name, cwd, &driver, model, effort, yolo, node);
    let session_id = entry.harness_session_id.clone().unwrap_or_default();
    let inserted = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
        if registry
            .entries
            .iter()
            .any(|existing| existing.name == entry.name)
        {
            return false;
        }
        if registry.entries.iter().any(|existing| {
            existing.harness_name() == "codex"
                && existing.harness_session_id.as_deref() == entry.harness_session_id.as_deref()
                && is_non_terminal(existing.status)
        }) {
            return false;
        }
        registry.entries.push(entry);
        true
    })
    .await;
    match inserted {
        Ok(true) => {}
        Ok(false) => {
            return Response::err(
                req.id,
                ErrorCode::AgentExists,
                format!("agent {name} or Codex thread {session_id} already exists"),
            )
        }
        Err(error) => {
            return Response::err(
                req.id,
                state_error_code(&error),
                format!("registry write: {error}"),
            )
        }
    }
    let handle = Arc::new(driver.into_actor(codex_thread_on_done(
        &ctx.emitter,
        ctx.home.registry_json(),
        name,
    )));
    ctx.codex_threads
        .lock()
        .await
        .insert(name.to_string(), Arc::clone(&handle));

    // The seed turn is just the first Submit in the actor's queue: no
    // dedicated task, no lock to steal. Its reply receiver is dropped on
    // purpose (nobody waits); the on-done hook still emits the event, and a
    // first follow-up ask STEERS into the seed turn instead of blocking
    // behind it (the daemon.rs:4077 mutex shape this replaces).
    //
    // A seedless spawn takes WARMUP_SEED rather than no turn at all (x-296f).
    // `thread/start` records a thread id but writes no rollout, and a harness
    // resolves a session to attach BY that rollout, so a worker with no turn
    // is a worker the operator cannot open: `codex resume` answers "no rollout
    // found for thread id <id>" (measured 2026-08-28, codex-cli 0.149.1).
    // One cheap turn buys attachability from the first second of a worker's
    // life, which is the window in which someone is most likely to look.
    let seed = if seed.trim().is_empty() {
        WARMUP_SEED.to_string()
    } else {
        seed
    };
    {
        let seed_name = name.to_string();
        let submitted = handle.submit(seed).await;
        if submitted.is_err() {
            let _ = ctx.emitter.emit(
                "daemon_recovery_error",
                &json!({"op": "codex_thread_seed", "name": seed_name, "error": "actor gone at seed submit"}),
            );
        }
    }
    // `substrate` and `cwd` are load-bearing: the mux restore receipt parser
    // (crates/fno/src/server.rs parse_spawn_receipts) drops any agent_spawned
    // event without both, which is how a thread worker could lose its only
    // resume fallback before the row is reaped.
    // `substrate` and `cwd` are load-bearing: the mux restore receipt parser
    // (crates/fno/src/server.rs parse_spawn_receipts) drops any agent_spawned
    // event without both, which is how a thread worker could lose its only
    // resume fallback before the row is reaped.
    let _ = ctx.emitter.emit(
        "agent_spawned",
        &json!({
            "name": name,
            "provider": "codex",
            "harness": "codex",
            "harness_session_id": session_id,
            "short_id": "",
            "status": "live",
            "lane": "thread",
            "substrate": "thread",
            "cwd": cwd.to_string_lossy(),
        }),
    );
    Response::ok(
        req.id,
        json!({
            "short_id": "",
            "harness": "codex",
            "harness_session_id": session_id,
            "session_id": session_id,
            "status": "live",
            "lane": "thread",
        }),
    )
}

/// (x-296f) The turn a seedless codex thread spawn takes so a rollout exists
/// and the worker is attachable immediately. Deliberately trivial: it must
/// cost one small turn and leave a transcript line an operator reads as
/// startup rather than as work someone asked for.
const WARMUP_SEED: &str = "Reply with the single word: ready.";

/// A Codex thread row startup recovery may auto-resume: it needs a full
/// durable identity AND a status that was non-terminal when the daemon died.
/// `handle_stop` marks a stopped thread `Exited`; resurrecting that row on the
/// next daemon start would silently undo `fno agents stop`.
fn codex_thread_recovery_candidate(entry: &RegistryEntry) -> bool {
    codex_thread_resume_identity(entry).ok().flatten().is_some() && is_non_terminal(entry.status)
}

async fn ensure_codex_thread_handle(
    ctx: &Ctx,
    entry: &RegistryEntry,
) -> Result<CodexThreadHandle, String> {
    if let Some(handle) = ctx.codex_threads.lock().await.get(&entry.name).cloned() {
        return Ok(handle);
    }
    let Some((session_id, cwd)) = codex_thread_resume_identity(entry)? else {
        return Err(format!("agent '{}' is not a Codex thread", entry.name));
    };
    // Connect OUTSIDE the lock. The resume now ensures the shared daemon
    // (which can take seconds to boot) and completes a network handshake, and
    // `codex_threads` is the map every other codex ask, stop and retask goes
    // through. Holding it across that await let one slow connect stall every
    // other codex thread on the machine, including the recovery loop.
    // The state-root grant (x-f22f) cannot be reconstructed here, and the loss
    // is announced rather than taken quietly. The roots reach a spawn from the
    // Python seam's `FNO_WORKER_ADD_DIRS`, which this long-lived shared daemon
    // does not have, and Rust deliberately runs no second copy of the resolver
    // (`writable_dirs.published_worker_writable_dirs`: one published value, two
    // readers). Reading this daemon's own env instead would grant whatever
    // shell started it, which is wrong in a more dangerous direction.
    //
    // In practice the grant usually survives: a turn-level `sandboxPolicy`
    // becomes the thread's default server-side, so a thread still loaded by the
    // codex app-server keeps it across an `fno-agents-daemon` restart. It is
    // lost only when the app-server itself restarted and reloaded the thread
    // from its rollout. That worker is then mute again, so it gets an event
    // instead of silence. The durable fix is a granted-roots receipt on the
    // registry row, which belongs to the sibling node that owns that schema.
    //
    // Emitted only for a thread that COULD have lost something. A yolo thread
    // is `danger-full-access` and needs no grant, and a resume that succeeds
    // says nothing on its own, so the event fires after the resume and only
    // for a bounded thread. An unconditional emit on every cache miss reports
    // a loss that never happened, which is the kind of telemetry an operator
    // learns to ignore.
    let bounded = !entry_posture_is_full_access(entry);
    let driver = crate::codex_thread::CodexThread::resume(
        cwd,
        &session_id,
        entry.model.as_deref(),
        entry_posture_is_full_access(entry),
        entry.effort.as_deref(),
    )
    .await
    .map_err(|error| format!("codex thread '{}' resume refused: {error}", entry.name))?;
    if bounded {
        let _ = ctx.emitter.emit(
            "codex_thread_resumed_without_state_grant",
            &json!({"name": entry.name, "lane": "thread", "session_id": session_id}),
        );
    }
    let mut threads = ctx.codex_threads.lock().await;
    // A concurrent caller may have won the race while we were connecting.
    // Theirs is already published, so keep it and drop ours: dropping a
    // driver closes one connection to the shared daemon and ends no thread.
    if let Some(handle) = threads.get(&entry.name).cloned() {
        return Ok(handle);
    }
    let handle = Arc::new(driver.into_actor(codex_thread_on_done(
        &ctx.emitter,
        ctx.home.registry_json(),
        &entry.name,
    )));
    threads.insert(entry.name.clone(), Arc::clone(&handle));
    Ok(handle)
}

/// Reconnect to Codex threads after daemon startup. Recovery first selects
/// rows by durable identity; this asynchronous pass reopens the shared-daemon
/// connections without delaying the supervisor's accept loop. The threads
/// themselves never stopped: the shared app-server daemon kept them.
fn schedule_codex_thread_recovery(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        recover_codex_threads(&ctx).await;
    });
}

/// The recovery pass body, split from the scheduler so a test can await it
/// (the spawned task is fire-and-forget). Resume-or-settle: a candidate that
/// resumes goes Live; one that fails is stamped Orphaned (AC15), never left
/// reading Live forever.
async fn recover_codex_threads(ctx: &Ctx) {
    {
        let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
            Ok(registry) => registry,
            Err(error) => {
                let _ = ctx.emitter.emit(
                    "daemon_recovery_error",
                    &json!({"op": "resume_codex_thread_registry", "error": error.to_string()}),
                );
                return;
            }
        };
        for entry in registry.entries {
            if !codex_thread_recovery_candidate(&entry) {
                continue;
            }
            match ensure_codex_thread_handle(&ctx, &entry).await {
                Ok(_handle) => {
                    let name = entry.name.clone();
                    let _ = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
                        if let Some(entry) = registry.find_mut(&name) {
                            // Same reason as `build_codex_thread_entry`: the
                            // thread owns no process, so its liveness cannot
                            // be a pid. Clear any stale one a pre-shared-daemon
                            // row still carries.
                            entry.pid = None;
                            entry.pid_start_time = None;
                            entry.status = AgentStatus::Live;
                        }
                    })
                    .await;
                }
                Err(error) => {
                    let _ = ctx.emitter.emit(
                        "daemon_recovery_error",
                        &json!({"op": "resume_codex_thread", "name": entry.name, "error": error}),
                    );
                    // A failed resume leaves the row readable Live forever
                    // unless it is settled here: Orphaned, because the
                    // rollout on disk is still the durable object a later
                    // resume (or a human) can pick up. Only a row that is
                    // still non-terminal is stamped - never overwrite a
                    // terminal status a concurrent stop just wrote.
                    let recover_name = entry.name.clone();
                    let _ = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
                        if let Some(entry) = registry.find_mut(&recover_name) {
                            if is_non_terminal(entry.status) {
                                entry.status = AgentStatus::Orphaned;
                            }
                        }
                    })
                    .await;
                }
            }
        }
    }
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

async fn handle_review_start(ctx: &Ctx, req: &Request) -> Response {
    let thread_id = match req.params.get("thread_id").and_then(Value::as_str) {
        Some(thread_id) if !thread_id.trim().is_empty() => thread_id,
        _ => return Response::err(req.id, ErrorCode::InvalidParams, "missing `thread_id`"),
    };
    let target_raw = match req.params.get("target").and_then(Value::as_str) {
        Some(target) if !target.trim().is_empty() => target,
        _ => return Response::err(req.id, ErrorCode::InvalidParams, "missing `target`"),
    };
    let target = match crate::codex_inject::parse_review_target(target_raw) {
        Some(target) => target,
        None => return Response::err(req.id, ErrorCode::InvalidParams, "invalid review target"),
    };
    let delivery = match req.params.get("delivery").and_then(Value::as_str) {
        Some("detached") => crate::codex_inject::ReviewDelivery::Detached,
        Some("inline") | None => crate::codex_inject::ReviewDelivery::Inline,
        Some(_) => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "delivery must be inline or detached",
            )
        }
    };
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(registry) => registry,
        Err(error) => return registry_read_failed(req.id, error),
    };
    let entry = match registry.find_name_or_full_session_id(thread_id) {
        Some(entry) => entry.clone(),
        None => {
            return Response::err(
                req.id,
                ErrorCode::AgentNotFound,
                format!("Codex thread {thread_id} not found"),
            )
        }
    };
    if !is_codex_thread_entry(&entry) {
        // The tight predicate, not the loose harness+host_mode one: a codex
        // PANE row passes the loose gate and then dies inside
        // ensure_codex_thread_handle with "is not a Codex thread" instead of
        // naming its real lane (AC16).
        if let Some(mux) = entry.mux.as_ref() {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "agent {} is a pane worker; review reaches no pane. Review a hosted \
                     thread (`--substrate thread`), or kill the pane: \
                     `fno mux pane kill {}:{}`.",
                    entry.name, mux.session, mux.pane_id
                ),
            );
        }
        return Response::err(
            req.id,
            ErrorCode::InvalidStatus,
            format!("agent {} is not a hosted Codex thread", entry.name),
        );
    }
    let handle = match ensure_codex_thread_handle(ctx, &entry).await {
        Ok(handle) => handle,
        Err(error) => return Response::err(req.id, ErrorCode::InvalidStatus, error),
    };
    let review = match handle.review(target, delivery).await {
        Ok(review) => review,
        Err(error) => return Response::err(req.id, ErrorCode::InvalidStatus, error),
    };
    Response::ok(
        req.id,
        json!({
            "turn_id": review.turn_id,
            "review_thread_id": review.review_thread_id,
            "harness_session_id": entry.harness_session_id,
        }),
    )
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

    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
    // A full-session-id token resolves here too: the client pre-check accepts
    // one, so a name-only lookup here would report a live agent as absent and,
    // with --provider, auto-spawn a duplicate row for the same session.
    let entry = match registry.find_name_or_full_session_id(&name) {
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

    if entry.harness_name() == "codex"
        && entry.host_mode_or_default() == crate::state::HOST_MODE_INTERACTIVE
    {
        // The loose predicate above routes a codex PANE row here too; only a
        // thread row (no short_id, no mux ref) belongs to the hosted lane.
        // A pane row would otherwise die inside ensure_codex_thread_handle
        // with the confusing "is not a Codex thread" refusal instead of
        // naming its real lane (AC16).
        if !is_codex_thread_entry(&entry) {
            if let Some(mux) = entry.mux.as_ref() {
                return Response::err(
                    req.id,
                    ErrorCode::InvalidStatus,
                    format!(
                        "agent {name} is a pane worker; ask reaches no pane. Send to the pane: \
                         `fno mux pane send {}:{} \"...\" --submit`, or ask a hosted thread.",
                        mux.session, mux.pane_id
                    ),
                );
            }
        } else {
            let handle = match ensure_codex_thread_handle(ctx, &entry).await {
                Ok(handle) => handle,
                Err(error) => return Response::err(req.id, ErrorCode::InvalidStatus, error),
            };
            // Submit + bounded wait: while a turn is driving this STEERS into
            // it instead of queueing behind a whole 600s turn, and a turn
            // longer than the bound answers `in_flight` (reply: null) with the
            // turn id - the old shape held a lock past the client's 120s
            // RESPONSE_DEADLINE and the ask failed silently while the daemon
            // kept driving. The reply is never lost: it persists in the
            // rollout and surfaces via `agent_ask_done` when the turn ends.
            let submitted = match handle.submit(message).await {
                Ok(reply_rx) => reply_rx,
                Err(error) => return Response::err(req.id, ErrorCode::InvalidStatus, error),
            };
            let turn = match tokio::time::timeout(crate::codex_thread::ask_wait(), submitted).await
            {
                Ok(Ok(Ok(receipt))) => receipt,
                Ok(Ok(Err(error))) => {
                    return Response::err(req.id, ErrorCode::Internal, error);
                }
                Ok(Err(_)) => {
                    return Response::err(
                        req.id,
                        ErrorCode::Internal,
                        "codex thread actor is gone",
                    );
                }
                Err(_) => {
                    return Response::ok(
                        req.id,
                        json!({
                            "reply": null,
                            "backend": "codex-thread",
                            "turn_id": handle.current_turn_id(),
                            "status": "in_flight",
                            "harness_session_id": entry.harness_session_id,
                        }),
                    );
                }
            };
            // The registry bump + agent_ask_done event fire from the actor's
            // on-done hook now, not here: an in_flight ask returns before the
            // turn ends, so this path only formats the answer.
            return Response::ok(
                req.id,
                json!({
                    "reply": turn.text,
                    "backend": "codex-thread",
                    "turn_id": turn.turn_id,
                    "status": turn.status,
                    "harness_session_id": entry.harness_session_id,
                }),
            );
        }
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
    // empty reply (silent-failure #4). Bounded like every worker ack: a wedged
    // worker answers "no write-ack" inside the window instead of parking the
    // handler forever.
    match tokio::time::timeout(
        WORKER_ACK_TIMEOUT,
        crate::protocol::read_response(&mut conn),
    )
    .await
    // Elapsed folds into the same error arm as a read fault.
    .unwrap_or(Err(crate::protocol::ProtocolError::UnexpectedEof))
    {
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
    let detector = provider_readiness_detector(entry.harness_name());
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
// handle_switchboard (agent.switchboard_v2 RPC; legacy alias agent.switchboard)
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

/// Flip the verified registry row to `Orphaned` after its drive fails. The
/// recipient can be restamped while a turn is in flight, so the mutation is an
/// identity CAS rather than a lookup by its reusable transport key.
async fn stamp_orphaned(
    home: &AgentsHome,
    name: String,
    identity: Value,
) -> Result<bool, state::StateError> {
    update_registry_offloaded(home.registry_json(), move |registry| {
        let Some(entry) = registry.find_mut(&name) else {
            return false;
        };
        if !switchboard_identity_matches(entry, &identity) {
            return false;
        }
        // Only flip a still-Live row. Do NOT clobber a terminal status the
        // worker already set (a clean `Exited` from stream.shutdown, or
        // `Failed`): clobbering Exited->Orphaned would make a deliberately
        // stopped session look adoptable (stream_worker.rs documents this hazard).
        if entry.status == AgentStatus::Live {
            entry.status = AgentStatus::Orphaned;
        }
        true
    })
    .await
}

/// Handle the identity-bound switchboard RPC.
///
/// Params: `{to: string, from: string, body: string, recipient_identity: object,
/// from_identity?: object, mirror?: bool, timeout_ms?: u64}`.
///
/// Result (Ok unless `to` is unknown or params invalid):
/// - `{delivered: true, identity_verified: true, reply, is_error, mirrored,
///   receipt, transport: "switchboard"}` — the turn was driven against B and
///   (when `mirror` and A is a held stream thread) B's reply was written into A.
/// - `{delivered: false, reason: "not-a-live-stream-thread"}` — B is not a held
///   stream-json thread; the caller demotes to the durable/socket path.
/// - `{delivered: false, reason: "<drive error>"}` — B was a stream thread but
///   the turn failed (broken pipe / orphaned / timeout); B is stamped orphaned
///   and A is NOT touched (the exchange did not complete).
///
/// Errors: `AgentNotFound` (unknown `to`), `InvalidParams` (missing/oversized).
/// A resolved registry row's identity, captured once and compared later to
/// confirm the SAME row across a lock gap. `switchboard_identity_matches`
/// and `handle_rm_with`'s retain both re-derived this comparison by hand
/// (self-review finding: two implementations of one operation, each missing
/// the field the other's calling context happened not to need); this is now
/// the one shared core both build a [`RowIdentity`] for and call.
///
/// A field left `None` here is UNASSERTED, not required-absent: `session_id`
/// in particular sits at `None` on a codex row until `late_bind_codex_sessions`
/// binds it, so a `None` captured before that bind must not read as a
/// mismatch against the SAME row's later `Some` (the finding #1 bug,
/// generalized here to the one place it also existed).
struct RowIdentity<'a> {
    harness: Option<&'a str>,
    name: Option<&'a str>,
    short_id: &'a str,
    session_id: Option<&'a str>,
    created_at: &'a str,
}

fn row_identity_matches(entry: &RegistryEntry, expected: &RowIdentity) -> bool {
    if let Some(harness) = expected.harness {
        if entry.harness_name() != harness {
            return false;
        }
    }
    if let Some(name) = expected.name {
        if entry.name != name {
            return false;
        }
    }
    if entry.short_id != expected.short_id {
        return false;
    }
    if let Some(session_id) = expected.session_id {
        if entry.harness_session_id.as_deref() != Some(session_id) {
            return false;
        }
    }
    entry.created_at == expected.created_at
}

fn switchboard_identity_matches(entry: &RegistryEntry, identity: &Value) -> bool {
    let Some(expected) = identity.as_object() else {
        return false;
    };
    let Some(harness) = expected.get("harness").and_then(Value::as_str) else {
        return false;
    };
    let Some(short_id) = expected.get("short_id").and_then(Value::as_str) else {
        return false;
    };
    let Some(created_at) = expected.get("created_at").and_then(Value::as_str) else {
        return false;
    };
    let session_id = match expected.get("session_id") {
        Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        _ => return false,
    };
    row_identity_matches(
        entry,
        &RowIdentity {
            harness: Some(harness),
            name: None,
            short_id,
            session_id,
            created_at,
        },
    )
}

/// Drive a mail body into a hosted codex thread through its actor. Delivered
/// means ACCEPTED (start/steer ack carries the turn id); the reply itself
/// surfaces later via `agent_ask_done`. Any acceptance failure demotes to the
/// durable path the same way the claude stream lane's drive failures do.
async fn deliver_to_codex_thread(
    ctx: &Ctx,
    req: &Request,
    to_entry: &RegistryEntry,
    from: &str,
    body: &str,
    timeout_ms: u64,
) -> Response {
    let to = to_entry.name.clone();
    let handle = match ensure_codex_thread_handle(ctx, to_entry).await {
        Ok(handle) => handle,
        Err(error) => {
            return Response::ok(
                req.id,
                json!({"delivered": false, "reason": format!("codex thread unavailable: {error}")}),
            )
        }
    };
    let (accept_tx, accept_rx) = tokio::sync::oneshot::channel();
    if let Err(error) = handle
        .submit_with_accept(body.to_string(), Some(accept_tx))
        .await
    {
        return Response::ok(req.id, json!({"delivered": false, "reason": error}));
    }
    // Acceptance is a start/steer ack (milliseconds in practice); the caller's
    // timeout_ms is the outer backstop, same shape as the claude drive bound.
    let outcome = match tokio::time::timeout(Duration::from_millis(timeout_ms), accept_rx).await {
        Ok(Ok(Ok(turn_id))) => Ok(turn_id),
        Ok(Ok(Err(reason))) => Err(reason),
        Ok(Err(_)) => Err("codex thread actor dropped the acceptance".into()),
        Err(_) => Err("turn acceptance timed out".into()),
    };
    match outcome {
        Ok(turn_id) => {
            let _ = ctx.emitter.emit(
                "agent_deliver_injected",
                &json!({
                    "name": to,
                    "from_name": from,
                    "provider": "codex",
                    "transport": "switchboard",
                    "turn_id": turn_id,
                }),
            );
            Response::ok(
                req.id,
                json!({
                    "delivered": true,
                    "identity_verified": true,
                    "transport": "switchboard",
                    "turn_id": turn_id,
                    "reply": null,
                }),
            )
        }
        Err(reason) => {
            let _ = ctx.emitter.emit(
                "agent_deliver_demoted",
                &json!({
                    "name": to,
                    "from_name": from,
                    "provider": "codex",
                    "transport": "switchboard",
                    "reason": reason,
                }),
            );
            Response::ok(req.id, json!({"delivered": false, "reason": reason}))
        }
    }
}

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
    let recipient_identity = match req.params.get("recipient_identity") {
        Some(value) if value.is_object() => value,
        _ => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "missing `recipient_identity`",
            )
        }
    };
    let from_identity = req.params.get("from_identity");
    if mirror && !from_identity.is_some_and(Value::is_object) {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "missing `from_identity` for mirrored switchboard turn",
        );
    }
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

    // x-4c87: a blind read must not claim a live recipient is absent. The
    // `unwrap_or_default()` this replaces made every mail send to a
    // demonstrably live worker print `agent '<name>' not found` first.
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
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
    if !switchboard_identity_matches(&to_entry, recipient_identity) {
        return Response::ok(
            req.id,
            json!({"delivered": false, "reason": "recipient-identity-changed"}),
        );
    }

    // A codex hosted thread is driven through its actor (x-de10): submit the
    // body and answer delivered on ACCEPTANCE - the protocol's own receipt
    // (turn/start ack when idle, steer ack when driving) - never a whole-turn
    // wait. The claude stream lane below waits out the turn because it mirrors
    // B's reply; a codex thread's reply surfaces later via `agent_ask_done`,
    // so there is nothing to mirror here.
    if is_codex_thread_entry(&to_entry) {
        return deliver_to_codex_thread(ctx, req, &to_entry, &from, &body, timeout_ms).await;
    }

    // B must be a held stream-json thread. A non-claude peer (PTY lane) or a
    // claude session with no live stream worker demotes to the durable path.
    let to_sock = ctx.home.worker_sock(&to_entry.short_id);
    if to_entry.harness_name() != "claude" || !is_live_stream_thread(&to_sock).await {
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
            match stamp_orphaned(&ctx.home, to.clone(), recipient_identity.clone()).await {
                Ok(true) => {}
                Ok(false) => {
                    let _ = ctx.emitter.emit(
                        "agent_deliver_status_write_failed",
                        &json!({
                            "name": to,
                            "from_name": from,
                            "provider": "claude",
                            "transport": "switchboard",
                            "reason": "recipient-identity-changed",
                        }),
                    );
                }
                Err(error) => {
                    let _ = ctx.emitter.emit(
                        "agent_deliver_status_write_failed",
                        &json!({
                            "name": to,
                            "from_name": from,
                            "provider": "claude",
                            "transport": "switchboard",
                            "reason": "registry-write-failed",
                            "error": error.to_string(),
                        }),
                    );
                }
            }
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
        // snapshot could point at A's old socket (gemini-review HIGH). A read
        // failure here DEMOTES the mirror the same way a mirror transport
        // failure does below: B's turn already completed, so failing the whole
        // request would discard a delivered reply and invite a duplicate
        // re-send (code-review on PR 924).
        let fresh = match load_registry_offloaded(ctx.home.registry_json()).await {
            Ok(r) => Some(r),
            Err(e) => {
                let _ = ctx.emitter.emit(
                    "agent_deliver_demoted",
                    &json!({
                        "name": from,
                        "from_name": to,
                        "transport": "switchboard-mirror",
                        // No provider field: a provider-named binding holding a
                        // harness literal is the axis-vocabulary violation the
                        // vocabulary contract prohibits.
                        "reason": format!("registry re-read failed: {e}"),
                    }),
                );
                None
            }
        };
        if let Some(from_entry) = fresh.as_ref().and_then(|f| f.find(&from)).filter(|entry| {
            from_identity.is_some_and(|identity| switchboard_identity_matches(entry, identity))
        }) {
            let from_sock = ctx.home.worker_sock(&from_entry.short_id);
            if from_entry.harness_name() == "claude" && is_live_stream_thread(&from_sock).await {
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
            "identity_verified": true,
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

/// Map a truth probe onto the wire value `list` renders.
///
/// Prefers the shared reachability verdict, which is derived once (Python-side,
/// `fno/agents/reachability.py`) with the falsifiers applied. The `state` arm
/// below is a COMPATIBILITY FALLBACK for a `fno` too old to emit the verdict,
/// not a second opinion: it maps transcript activity alone, so a session whose
/// process died forty minutes ago still reads `working` there and renders live.
///
/// Note the fallback and the verdict disagree deliberately on quiet rows. The
/// fallback calls a silent row `orphaned`; the verdict calls it `unknown`,
/// because silence is absence of evidence and this registry lists REACHABLE
/// agents rather than live processes -- a row is never condemned for being
/// quiet, only for an affirmative falsification.
///
/// STATUS is served ACTIVITY, so a confirmed-live pid is not an input here
/// (x-c672): a process being up says nothing about when its transcript last
/// moved, and the Python list lane has no pid census, so a pid lift here would
/// read the same row as two different words on the two lanes. An unanswered
/// activity age is `unknown` on both.
fn row_timestamp(value: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    let value = value?;
    if let Some(raw) = value.as_str() {
        return chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|parsed| parsed.with_timezone(&chrono::Utc));
    }
    let micros = value.as_u64()?;
    if micros <= 1_000_000_000_000 {
        return None;
    }
    chrono::DateTime::from_timestamp_micros(micros as i64)
}

/// Refuse a row-level verdict when the same emitted row carries fresher
/// evidence against it. This is deliberately pure and shared by the fixture
/// test with Python; the caller supplies all fields before the row is written.
/// `now` is injected so the fixture's fixed clock and production's wall clock
/// assert the same rules.
fn apply_row_contradiction(row: &mut Map<String, Value>, now: chrono::DateTime<chrono::Utc>) {
    // The falsifier as it ARRIVED, snapshotted before any rule below rewrites
    // `basis`. Python's `_supervisor_contradicted` reads the input mapping, so
    // reading the mutated map here would name a different falsifier than the
    // twin for the same row, and the shared fixture has no case where two
    // rules fire together to catch it.
    let incoming_basis = row
        .get("basis")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // `status` for the same reason, and the reason generalises: Python reads
    // the input `row` and writes a SEPARATE `projected` dict, so every rule
    // there sees the original. This twin mutates `row` in place, so any rule
    // reading `status` after an earlier one rewrote it diverges from Python
    // for that row. Today the two rules are mutually exclusive - `terminal`
    // is `orphaned`/`exited` and this one needs `spawning` - so nothing
    // changes; snapshot anyway, because relying on that exclusion is a rule
    // no test states and the next rule added here will not know it.
    let incoming_status = row
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let event_at = row_timestamp(row.get("last_event_at"));
    let reconciled_at = row_timestamp(row.get("last_reconciled_at"));
    let terminal = matches!(
        row.get("status").and_then(Value::as_str),
        Some("orphaned" | "exited")
    );
    if terminal && event_at.is_some() && reconciled_at.is_some() && event_at > reconciled_at {
        row.insert("status".into(), json!("unknown"));
        row.insert("basis".into(), json!("stale-verdict-fresher-event"));
    }

    let message_at = row_timestamp(row.get("last_message_at"));
    let message_is_too_new = match (message_at, event_at) {
        (Some(message), Some(event)) => message - event > chrono::Duration::seconds(2),
        _ => false,
    };
    if message_is_too_new {
        row.insert("last_message_at".into(), Value::Null);
        row.insert(
            "last_message_at_basis".into(),
            json!("refused-newer-than-transcript"),
        );
    }

    // (x-d401) A stored `spawning` token a live pid has outlived: the token
    // stopped being a measurement. Fires only on POSITIVE liveness (the
    // caller measured a live pid and injected `pid_alive: true`); unknown
    // keeps the token, and a missing `created_at` is absent age evidence,
    // not staleness. Mirrors `_spawning_outlived_by_a_live_pid` in Python;
    // rows read `spawning` for 3-16 hours while alive (x-0248).
    if incoming_status == "spawning"
        && row.get("pid_alive") == Some(&Value::Bool(true))
        // `> Duration::seconds(600)`, not `num_seconds() > 600`: num_seconds
        // truncates, so a 600.5s-old row read `spawning` here and `live` in
        // Python, whose timedelta compare keeps the fraction.
        && row_timestamp(row.get("created_at"))
            .is_some_and(|created_at| now - created_at > chrono::Duration::seconds(600))
    {
        row.insert("status".into(), json!("quiet"));
        row.insert("basis".into(), json!("stale-spawning-live-pid"));
    }
    row.remove("pid_alive");

    // Both keys ALWAYS ride the row, as `reachability`/`basis` and
    // `progress`/`progress_basis` already do on this same row. A conditional
    // key cannot be told apart from a producer that forgot to set one, and the
    // list-row contract in schemas/agents-list-row.json is an exact key set.
    let (origin, origin_basis) = liveness_origin(row);
    row.insert("liveness_origin".into(), origin);
    row.insert(
        "liveness_origin_basis".into(),
        origin_basis.map_or(Value::Null, |basis| json!(basis)),
    );
    // (x-d401, x-d4a6) A superseded supervisor claim beside the falsifier
    // that beat it: `superseded_live_status` is a caller-injected input (like
    // `pid`), popped here; only the basis key survives, null when no
    // supersession happened. PRESENCE is the caller's assertion that a
    // supersession happened; which words claim nothing lives in the gate
    // that stamps this input (read.py's {idle, done} admission set) - a
    // second word list here once denied a supersession the gate had stamped.
    // Mirrors `_supervisor_contradicted` in Python.
    let superseded = row
        .get("superseded_live_status")
        .and_then(Value::as_str)
        .is_some_and(|word| !word.is_empty());
    let contradicted = superseded
        && row.get("reachability").and_then(Value::as_str) == Some("unreachable")
        && !incoming_basis.is_empty();
    row.insert(
        "live_status_basis".into(),
        if contradicted {
            json!(format!("contradicted-by-{incoming_basis}"))
        } else {
            Value::Null
        },
    );
    row.remove("superseded_live_status");
}

/// Parse one row timestamp into `(value, basis)`, separating absent from
/// unreadable. Mirrors `_read_field` in cli/src/fno/agents/row_contradiction.py.
///
/// Folding the two together is what let `liveness_origin: null` mean five
/// different things at once, so a reader holding one null could not tell
/// "nothing was recorded" from "something this parser cannot read".
fn row_field_with_basis(
    row: &Map<String, Value>,
    key: &str,
    label: &str,
) -> (Option<chrono::DateTime<chrono::Utc>>, Option<String>) {
    match row.get(key) {
        None | Some(Value::Null) => (None, Some(format!("{label}-absent"))),
        raw => match row_timestamp(raw) {
            Some(parsed) => (Some(parsed), None),
            None => (None, Some(format!("{label}-unreadable"))),
        },
    }
}

/// Return `(liveness_origin, basis)` for one row. Mirrors `_liveness_origin`
/// in cli/src/fno/agents/row_contradiction.py, and the shared fixture at
/// schemas/agents-row-contradiction.json drives both.
///
/// THE PID GATE COMES FIRST. This producer already checked it and the Python
/// one did not, so a pidless row read `survivor` there and null here: one
/// field, two reachable implementations, one guard. A non-null origin carries
/// no basis, because the value is its own evidence.
fn liveness_origin(row: &Map<String, Value>) -> (Value, Option<String>) {
    if !row.get("pid").is_some_and(|value| !value.is_null()) {
        return (Value::Null, Some("pid-absent".to_string()));
    }
    let (created_at, basis) = row_field_with_basis(row, "created_at", "created-at");
    let Some(created_at) = created_at else {
        return (Value::Null, basis);
    };
    let (pid_started_at, basis) = row_field_with_basis(row, "pid_start_time", "pid-start");
    let Some(pid_started_at) = pid_started_at else {
        return (Value::Null, basis);
    };
    if (pid_started_at - created_at).num_seconds() > 600 {
        (json!("resumed"), None)
    } else {
        (json!("survivor"), None)
    }
}

/// The STATUS word `list` renders: SERVED ACTIVITY, never a `live` token
/// (x-c672, AC7). Nothing decides on this word anymore - retirement reads the
/// reverse join, the lanes read their own probes - so the column answers the
/// operator's actual question, what is this session doing: `writing` (the
/// transcript moved inside `STALE_ATTENTION_S`), `quiet` (older), `parked`
/// (the tail closed a promise). A positively falsified row reads `orphaned`,
/// and a probe that did not answer reads `unknown`. A confirmed-live pid does
/// NOT lift an unanswered age to `quiet`: the word is activity, and a process
/// being up says nothing about when it last wrote - the same row must render
/// the same word through the Python list lane, which has no pid census.
fn rendered_status_from_truth(probe: Option<&crate::truth_probe::TruthProbe>) -> &'static str {
    if probe.and_then(|p| p.reachability.as_deref()) == Some("unreachable") {
        return "orphaned";
    }
    match probe.map(|p| p.state.as_str()) {
        Some("done") => "parked",
        Some(_) => match probe.and_then(|p| p.last_activity_age_s) {
            Some(age) if age < STALE_ATTENTION_S => "writing",
            Some(_) => "quiet",
            None => "unknown",
        },
        None => "unknown",
    }
}

/// True for a Claude model id or tier alias. Mirrors
/// `fno.agents.model_routing.is_anthropic_model` (cli/src/fno/agents/model_routing.py) --
/// duplicated rather than shelled out to because the daemon already pays one
/// probe per row and a second process spawn per row would multiply that cost
/// for a four-branch string check.
fn is_anthropic_model(model: &str) -> bool {
    let name = model.trim().to_ascii_lowercase();
    name.starts_with("claude-") || matches!(name.as_str(), "opus" | "sonnet" | "haiku" | "fable")
}

/// Structural refusal predicate (Locked Decision 3). Mirrors
/// `fno.agents.reachability._is_refused` -- never reads the transcript's
/// prose, so a reworded refusal message cannot break it. Fails OPEN: a
/// recorded `route_settings_path` records the INTENDED route, so a
/// foreign-routed worker answering as a foreign model is healthy, not refused.
fn is_refused(observed_model: &Value, harness: &str, route_settings_path: Option<&str>) -> bool {
    if harness != "claude" {
        return false;
    }
    if route_settings_path.is_some() {
        return false;
    }
    if observed_model.get("kind").and_then(Value::as_str) != Some("observed") {
        return false;
    }
    match observed_model.get("model").and_then(Value::as_str) {
        Some(model) => !is_anthropic_model(model),
        None => false,
    }
}

/// Map a truth probe onto the progress axis `list` renders, mirroring Python's
/// `classify_progress` (`fno/agents/reachability.py`). Reads the SAME probe
/// `rendered_status_from_truth` reads, plus `harness` and
/// `route_settings_path` off the registry entry -- no second probe is paid.
///
/// Precedence matches the Python classifier exactly: a falsified/unresolved
/// row first (`reachability` absent or `unreachable` -- AC12-FR, the
/// compatibility-fallback case included, since an unmeasured row has no
/// progress state to report either), then the refusal predicate, then the
/// truth-state arms plus the measured transcript age. A written `working`
/// state is not progress evidence when its transcript stopped advancing.
pub(crate) fn progress_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
    harness: &str,
    route_settings_path: Option<&str>,
) -> (&'static str, &'static str) {
    match probe.and_then(|p| p.reachability.as_deref()) {
        Some("unreachable") | None => return ("unknown", "no-evidence"),
        _ => {}
    }
    let observed_model = probe.map(|p| &p.observed_model);
    if observed_model.is_some_and(|om| is_refused(om, harness, route_settings_path)) {
        return ("refused", "model-refused");
    }
    match probe.map(|p| p.state.as_str()) {
        Some("working" | "watching") => match probe.and_then(|p| p.last_activity_age_s) {
            None => ("unknown", "no-evidence"),
            Some(age) if age >= STALE_ATTENTION_S => ("unknown", "silent"),
            Some(_) => ("advancing", "transcript-turn"),
        },
        Some("your-move") => ("awaiting-operator", "operator-turn"),
        Some("done") => ("parked", "promise"),
        Some("stalled") => ("unknown", "silent"),
        _ => ("unknown", "no-evidence"),
    }
}

pub(crate) fn registry_truth_handle(entry: &RegistryEntry) -> String {
    if let Some(session_id) = entry.harness_session_id.as_deref() {
        return session_id.to_string();
    }
    if !entry.short_id.is_empty() {
        entry.short_id.clone()
    } else {
        entry.name.clone()
    }
}

/// The attention window this surface orders by. Session-truth's stall window
/// is 7200s and correct FOR REAPING; for display it is exactly the gap a
/// dead-under-two-hours worker hides in, so the ordering window is ten
/// minutes. Mirrors `session_truth.STALE_ATTENTION_S` and the mux client's
/// constant - the three cannot share code (the crates do not link), so the
/// shared fixture in schemas/ is what pins them together.
const STALE_ATTENTION_S: f64 = 600.0;

const LIST_PROJECTION_OMISSIONS: [&str; 2] = ["model", "model_basis"];

/// `agent.list`, with the truth probe injected as a BATCH seam: one call for
/// the whole filtered page, keyed by handle. The per-row seam it replaced spent
/// one Python interpreter cold start per row per list.

fn handle_list_with_truth<F>(ctx: &Ctx, req: &Request, truth_fn: F) -> Response
where
    F: Fn(&[String]) -> std::collections::HashMap<String, crate::truth_probe::TruthProbe>,
{
    let all = req
        .params
        .get("all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Task 3.1: accept cwd/provider/status/progress filters matching Python list_agents.
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
    let filter_progress = req
        .params
        .get("progress")
        .and_then(|v| v.as_str())
        .map(String::from);
    let cwd_project = req
        .params
        .get("project_root")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Reject an invalid --status up front so a typo fails fast with exit 13
    // instead of silently returning zero rows + exit 0 (Codex P2 on PR #361).
    // Mirrors Python's AgentStatusFilter enum, which Typer
    // rejects at parse time.
    if let Some(ref st) = filter_status {
        if !matches!(
            st.as_str(),
            "writing" | "quiet" | "parked" | "orphaned" | "unknown"
        ) {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "invalid --status '{st}' (expected: writing | quiet | parked | orphaned | unknown)"
                ),
            );
        }
    }
    if let Some(ref progress) = filter_progress {
        if !matches!(
            progress.as_str(),
            "advancing" | "awaiting-operator" | "parked" | "refused" | "unknown"
        ) {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "invalid --progress '{progress}' (expected: advancing | awaiting-operator | parked | refused | unknown)"
                ),
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

    // x-4c87: an unreadable registry is an RPC error, never a valid empty
    // roster with discovered-only rows beside it. `unwrap_or_default()` here is
    // what let a broken registered lane publish `count: 0` next to a healthy
    // `discovered_count` and read as "no agents". This handler is sync, so it
    // takes the asserted blocking read inline.
    let registry = match load_registry_asserted(&ctx.home.registry_json()) {
        Ok(reg) => reg,
        Err(e) => return registry_read_failed(req.id, e),
    };
    let filtered: Vec<_> = registry
        .entries
        .iter()
        .filter(|e| {
            if !all {
                if let Some(ref p) = cwd_project {
                    if &e.project_root != p {
                        return false;
                    }
                }
            }
            if let Some(ref cwd) = filter_cwd_norm {
                if &norm_path(&e.cwd) != cwd {
                    return false;
                }
            }
            if let Some(ref prov) = filter_provider {
                // The provider filter reads the v15+ vendor axis, never the
                // harness: comparing harness_name() here dropped a worker
                // hosted on one CLI and routed to another vendor.
                if e.provider.as_deref() != Some(prov.as_str()) {
                    return false;
                }
            }
            true
        })
        .collect();
    // ONE probe call for the whole page. This handler renders `state`,
    // `observed_model` and the reachability triple into every row, so it needs
    // a real reading per row and no stat can stand in for one: a grown
    // transcript means the tail CHANGED, which makes the last reading stale
    // rather than confirmed. Batching is the whole win here, and it is enough -
    // 24 rows cost 18.7 s as per-row subprocesses and 0.8 s as one.
    //
    // Duplicate handles across rows collapse in the request and fan back out on
    // read: free deduplication the per-row path never had.
    let handles: Vec<String> = {
        let mut seen = std::collections::BTreeSet::new();
        filtered
            .iter()
            .map(|e| registry_truth_handle(e))
            .filter(|h| seen.insert(h.clone()))
            .collect()
    };
    let truths = truth_fn(&handles);
    let classified: Vec<_> = filtered
        .into_iter()
        .map(|e| {
            // A handle absent from the batch behaves exactly as a `None` probe
            // did on the per-row path: every reader below already treats an
            // unanswered row that way.
            let truth = truths.get(&registry_truth_handle(e)).cloned();
            let rendered_status = rendered_status_from_truth(truth.as_ref());
            // The whole reachability triple, not just the verdict that
            // `rendered_status` above was picked from. That rendered word says
            // WHAT the row is; the triple says which question was answered and
            // off what evidence, and only the triple separates a positive
            // transcript reading from a fired falsifier. Null on a `fno` too old
            // to emit them: a stale probe that did not answer must read as
            // absent, never as no-evidence.
            let evidence = (
                json!(truth.as_ref().and_then(|t| t.reachability.as_deref())),
                json!(truth.as_ref().and_then(|t| t.basis.as_deref())),
                json!(truth.as_ref().and_then(|t| t.last_activity_age_s)),
                json!(truth.as_ref().and_then(|t| t.last_event_at.as_deref())),
                json!(truth.as_ref().and_then(|t| t.last_message.as_deref())),
            );
            // The orthogonal axis: reachability answers "can I reach this
            // process"; progress answers "is it advancing, awaiting the
            // operator, parked, or refused" -- read off the SAME probe, so a
            // refused-but-reachable row is never rendered as a fourth
            // reachability value.
            let (progress, progress_basis) = progress_from_truth(
                truth.as_ref(),
                e.harness_name(),
                e.route_settings_path.as_deref(),
            );
            // A probe that did not answer is the same situation Python's
            // resolver reports as `no-transcript` (its dominant cause here is
            // the routine exit-13 miss), so both emitters say the same thing
            // about the same row instead of one of them inventing a null.
            let observed_model = truth
                .map(|t| t.observed_model)
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| json!({"kind": "no-transcript"}));
            (
                e,
                rendered_status,
                observed_model,
                evidence,
                progress,
                progress_basis,
            )
        })
        .collect();
    let mut entries: Vec<Value> = classified
        .into_iter()
        .filter(
            |(_e, rendered_status, _observed, _evidence, progress, _progress_basis)| {
                if let Some(ref st) = filter_status {
                    if rendered_status != &st.as_str() {
                        return false;
                    }
                }
                if let Some(ref want) = filter_progress {
                    if progress != want {
                        return false;
                    }
                }
                true
            },
        )
        .map(
            |(e, rendered_status, observed_model, evidence, progress, progress_basis)| {
                let (reachability, basis, last_activity_age_s, last_event_at, last_message) =
                    evidence;
                // Return the full row shape matching Python's serialize_entry. The
                // key set is pinned by schemas/agents-list-row.json, asserted here
                // and by the Python test; edit that file before adding a key.
                // Fields present in RegistryEntry are mapped directly; fields absent from
                // the Rust registry are emitted as null with a NOTE citing the carveout.
                //
                // live_status remains null because the daemon does not duplicate the
                // harness supervisor view. `status`, however, is the family-1
                // transcript verdict attached above; stored registry status is only
                // lifecycle metadata and cannot prove read-side liveness or death.
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
                let resume_id: Option<String> = match e.harness_name() {
                    "claude" => e
                        .transport_short()
                        .map(str::to_string)
                        .or_else(|| e.session_id.clone()),
                    "codex" => e.codex_session_id.clone().or_else(|| e.session_id.clone()),
                    "gemini" => e.gemini_session_id.clone().or_else(|| e.session_id.clone()),
                    // Python writes opencode ids to the canonical harness_session_id
                    // and drops `session_id` on write (it is Rust-set only), so
                    // falling through would report null for every opencode row. Same
                    // resolution as `to_agent_entry` and `client_verbs::session_id_field`.
                    "opencode" => e
                        .harness_session_id
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| e.session_id.clone()),
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
                // The mailbox address, mirroring `fno.agents.format.row_address`.
                // This projection is the one `fno agents list` takes whenever an
                // installed binary is present, so a column emitted Python-side only
                // would be missing from the path nearly every reader uses -- which
                // is exactly how this row shape drifted before. `short_id` is a
                // fallback for claude ONLY, where the transport key IS the first
                // eight; elsewhere it is a daemon worker key and would advertise a
                // mailbox nothing drains.
                let address: Value = e
                    .harness_session_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(canonical_handle(s)))
                    .or_else(|| {
                        if e.harness_name() == "claude" {
                            e.transport_short().map(|s| Value::String(s.to_string()))
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Value::Null);
                // Same formatter as Python's `AgentEntry.crown_label`, so the two
                // surfaces render an identical descriptor for the same row. Python
                // tests the scope for falsiness (`self.crown_scope or '?'`), so the
                // empty string has to fall back here too, not just None.
                let crown: Value = match e.crown_level {
                    Some(level) => Value::String(format!(
                        "L{level} {}",
                        e.crown_scope
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("?")
                    )),
                    None => Value::Null,
                };
                let mut row = json!({
                    "name": e.name,
                    // `harness` is the sole identity axis, and it names the CLI,
                    // never the model vendor. `provider` beside it is the v15+
                    // model-vendor axis stamped at spawn; the pre-split alias that
                    // carried the harness value under this name stayed omitted
                    // until x-f273, which hid the real vendor axis from every
                    // RPC consumer. `observed_model` below remains the honest
                    // answer to what actually answered.
                    "harness": e.harness_name(),
                    "provider": e.provider,
                    // Stored effort is a separate spawn axis. It is passed
                    // through unchanged; observed_model remains transcript truth.
                    "effort": e.effort,
                    "harness_session_id": e.harness_session_id,
                    // The two identity axes plus classified lineage (x-dfe7),
                    // mirroring Python's serialize_entry: `thread_id` is the
                    // stable fno identity, `current_session_id` the address
                    // delivery follows now, and the predecessor chain / fork
                    // edge are the retained history. Emitted as separate keys
                    // so a renderer cannot present a retired id as current.
                    "thread_id": e.fno_id,
                    "current_session_id": e.harness_session_id,
                    // The node this row works, already stamped in registry
                    // storage from resolved spawn provenance. Never infer it
                    // from the row name.
                    "node": e.node,
                    "predecessor_session_ids": e.predecessor_session_ids,
                    "forked_from_session_id": e.forked_from_session_id,
                    "short_id": short_id,
                    "session_id": session_id,
                    "address": address,
                    "cwd": e.cwd,
                    "created_at": e.created_at,
                    "last_message_at": e.last_message_at,
                    "last_message_at_basis": null,
                    "last_reconciled_at": e.last_reconciled_at,
                    // The SERVED liveness pair, written only by the
                    // sweep: a reader trusts it while the stamp is young and
                    // reads its age honestly when it is not.
                    "liveness": e.liveness,
                    "liveness_measured_at": e.liveness_measured_at,
                    // The harness's own title for the session, served
                    // from the probe's fresh reading; a probe that ANSWERED
                    // None is trusted (the harness carries no title now, e.g.
                    // a rotated transcript), and the sweep's stored last-seen
                    // value stands only for a row the batch never measured.
                    // Beside `name`, never in it: the label is fno's, the
                    // title is the harness's.
                    "harness_title": truths
                        .get(&registry_truth_handle(e))
                        .map(|t| t.harness_title.clone())
                        .unwrap_or_else(|| e.harness_title.clone()),
                    "status": rendered_status,
                    // The reachability triple, from the same probe the rendered
                    // word above came from. `fno agents list` is where `peek` and
                    // the census helpers below send a reader for this evidence, and
                    // the default `list` is THIS projection whenever an installed
                    // binary is present -- so emitting it Python-side only left the
                    // documented field missing on the path readers actually take.
                    "reachability": reachability,
                    "basis": basis,
                    // The orthogonal progress axis, from the same probe as the
                    // reachability triple above (fno.agents.reachability.classify_progress).
                    "progress": progress,
                    "progress_basis": progress_basis,
                    "last_activity_age_s": last_activity_age_s,
                    // The absolute stamp of the newest transcript activity and the
                    // flattened LAST-turn text, from the same probe as the age -
                    // the pair that makes a wedged-but-`working` row visible. Null
                    // when the probe never answered, which an absent reading must
                    // render as, never a fresh one.
                    "last_event_at": last_event_at,
                    "last_message": last_message,
                    "live_status": null,
                    // The model this worker is ACTUALLY answering as, from the same
                    // family-1 probe that produced `status` above -- so the daemon
                    // never grows a second transcript reader that could disagree
                    // with the truth verb about the same session.
                    "observed_model": observed_model,
                    // v23 (x-2019): the stored REQUEST beside the observation,
                    // plus the substitution marker derived from the payload
                    // above - the same two keys, computed the same way, as
                    // Python's serialize_entry. Null marker is match-or-
                    // unknown; it never reads as a clean bill on its own.
                    "requested_model": e.requested_model,
                    "model_substituted": model_substitution_marker(
                        e.requested_model.as_deref(),
                        &observed_model,
                    ),
                    // Architecture C (plan ab-70faa65b): additive keys, never removing
                    // live_status (Locked #4 back-compat). `pid` is the worker pid for
                    // a PTY agent, null for a one-shot ask (no managed process). The
                    // pid is cleared when a PTY row reconciles to exited (Locked #7),
                    // so it never lingers as a misleading liveness signal.
                    // `last_reconciled_at` is the raw RFC3339 of the last probe (null
                    // when never reconciled); the client renders it as the CHECKED age.
                    "pid": e.pid,
                    "pid_start_time": e.pid_start_time,
                    "log_path": log_path,
                    // The mux hosting ref ({session, pane_id}) for a pane-hosted row,
                    // else null. A pane row's short_id is empty, so this is the only
                    // key that says where such a worker actually lives; without it a
                    // caller reads a bound pane worker as unhosted.
                    "mux": e.mux,
                    // (x-7955) The lane the row was spawned on, read from the
                    // registry record. Never inferred from `mux` or
                    // `thread_id`: a paneless pane row and a thread row would
                    // then read identically, which is the confusion a reader
                    // cannot recover from.
                    "substrate": e.substrate,
                    // Crown (US9): the compact descriptor plus the raw fields, so a
                    // minion can resolve who to escalate to.
                    "crown": crown,
                    "crown_level": e.crown_level,
                    "crown_scope": e.crown_scope,
                    "crown_grantor": e.crown_grantor,
                    // The parent edge the orphan check keys on (same key as
                    // Python's serialize_entry); null is a real answer.
                    "spawned_by_session": e.spawned_by_session,
                    // How this session came to exist: "operator" for one a human
                    // started by hand, "spawn" for a footnote-created worker, null
                    // for a row nothing stamped. Emitted on BOTH serializers because
                    // `fno agents list` auto-routes to this projection whenever an
                    // installed binary is present, so a Python-only key would be
                    // missing from the path nearly every reader takes (x-944f).
                    "origin": e.origin,
                    // x-481e: the row's mail delivery policy ("bus-only" holds
                    // mail on the durable bus, null is the injectable default).
                    // Stored since v14, read by every injector gate, and until
                    // now never rendered anywhere a human or a king could see
                    // it. The remaining time on a timed hold is Python-only
                    // (`dnd` in schemas/agents-list-row.json): its clock lives
                    // under fno's config-resolved state dir, which the daemon
                    // does not load.
                    "delivery_policy": e.delivery_policy,
                    // Superset of Python's serialize_entry: project_root is retained
                    // as the daemon's native grouping key (existing daemon_e2e
                    // contract) alongside the shared parity fields. Python list
                    // has no project_root; the extra key is a harmless superset.
                    "project_root": e.project_root,
                });
                if let Some(object) = row.as_object_mut() {
                    // No `pid_alive` injection here: this row's `status` is
                    // `rendered_status`, which `rendered_status_from_truth`
                    // draws from a closed set of live/orphaned/unknown, so the
                    // stale-spawning rule cannot match it. Injecting the input
                    // would be a guard that never fires, reading as coverage.
                    // The rule's one live carrier is Python's
                    // `spawn_gate.census` (`fno agents top`), which measures
                    // liveness itself and renders the stored token.
                    apply_row_contradiction(object, chrono::Utc::now());
                    object.remove("pid_start_time");
                }
                row
            },
        )
        .collect();
    // Attention order: evidence of neglect first, the same order the mux
    // table and the Python list lane apply (all three assert against one
    // shared fixture). Registry insertion order said nothing about who needs
    // the operator; the row's own `status` word is a low-pass filter that
    // reads `live` for a worker dead under two hours, so it is barred from
    // the key. The needs-me fold rank does not ride this surface (it is a
    // mux-client concept); rows sort on their evidence alone.
    entries.sort_by(|a, b| attention_sort_key(a).cmp(&attention_sort_key(b)));
    // Echo the filters the daemon applied so `list --json` self-describes its
    // query, matching Python `read.list_agents`'s `filters_applied` (sigma-review:
    // the client previously always fell back to an all-null block because the
    // daemon omitted this field). `cwd` is the value the client sent; absolute
    // resolution to match Python's `Path(cwd).resolve()` is deferred (cv-eeaad75d).
    let filters_applied = json!({
        "cwd": filter_cwd_norm,
        "provider": filter_provider,
        "status": filter_status,
        "progress": filter_progress,
    });
    Response::ok(
        req.id,
        json!({
            "agents": entries,
            "filters_applied": filters_applied,
            "fields_omitted": LIST_PROJECTION_OMISSIONS,
        }),
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
///
/// `agents.by_status` is a histogram of the STORED lifecycle enum -- what was
/// last WRITTEN to each registry row -- and NOT a reachability census. It will
/// not match `fno agents list`, and that is correct rather than a bug: they
/// answer different questions. Because the daemon reconciles once at startup,
/// these values can be stale for its entire uptime, so an `exited` here means
/// "we recorded exited at some point", not "unreachable now". For reachability,
/// read `fno agents list` (its `reachability` + `basis` fields), which derives
/// from `cli/src/fno/agents/reachability.py`.
///
/// Deliberately NOT renamed to say so: the field name is pinned by the schema
/// and its CI parity check, and a breaking rename would buy wording alone.
async fn handle_status(ctx: &Ctx, req: &Request) -> Response {
    // load_registry does blocking flock I/O; offload it from the async worker
    // thread (Gemini review). The drive-table read below stays async. A read
    // failure is an RPC error (x-4c87): `unwrap_or_default()` here published
    // zero-agent status counts over a broken registry.
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(reg) => reg,
        Err(e) => return registry_read_failed(req.id, e),
    };
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

/// Resolve lifecycle tokens through the all-source client resolver. Return the
/// resolved row itself because the helper may have just adopted a store-only
/// session that is absent from the caller's pre-heal registry snapshot.
async fn entry_for_lifecycle(
    registry: &state::Registry,
    token: &str,
    registry_path: &std::path::Path,
) -> Result<Option<RegistryEntry>, String> {
    let Value::Array(rows) = serde_json::to_value(&registry.entries)
        .map_err(|exc| format!("could not inspect registry identities: {exc}"))?
    else {
        return Err("could not inspect registry identities".to_string());
    };
    let worker_token = token.to_string();
    let path = registry_path.to_path_buf();
    let resolved = tokio::task::spawn_blocking(move || {
        crate::client_verbs::resolve_entry_with_heal(&rows, &worker_token, &path)
    })
    .await
    .map_err(|exc| format!("identity resolution task failed: {exc}"))?;
    match resolved {
        Ok(entry) => {
            let mut entry: RegistryEntry = serde_json::from_value(entry)
                .map_err(|exc| format!("resolved identity row is unreadable: {exc}"))?;
            entry.backfill_harness_aliases();
            if let Some(legacy) = entry.backfill_short_id() {
                return Err(format!(
                    "resolved identity row {:?} has conflicting transport ids (legacy={legacy:?})",
                    entry.name
                ));
            }
            Ok(Some(entry))
        }
        Err(crate::client_verbs::ResolveError::NotFound(_)) => Ok(None),
        Err(err) => Err(err.message()),
    }
}

async fn handle_stop(ctx: &Ctx, req: &Request) -> Response {
    let requested_name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `name`"),
    };
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
    let entry =
        match entry_for_lifecycle(&registry, &requested_name, &ctx.home.registry_json()).await {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return Response::err(
                    req.id,
                    ErrorCode::AgentNotFound,
                    format!("agent {requested_name} not found"),
                )
            }
            Err(message) => return Response::err(req.id, ErrorCode::InvalidParams, message),
        };
    let name = entry.name.clone();
    if entry.status == AgentStatus::Exited {
        // An exited agent needs no stop work. (Pre-G4 this also force-cleared a
        // lingering WebSocket driver; the drive surface was retired at G4.)
        return Response::ok(
            req.id,
            json!({"already_exited": true, "short_id": entry.short_id}),
        );
    }
    // A pane-hosted row's ONE live ref is the mux pane (state.rs invariant:
    // mux XOR worker-socket identity XOR bg thread), and `stop` reaches no pane.
    // Answering it with a success would report work this verb did not perform
    // over a live pane - the zombie shape. Refuse and name the working verb with
    // the row's own ref, in handle_rm's refusal voice. Keys on `entry.mux`,
    // never the harness, so it covers claude, codex, opencode, and agy pane
    // rows in one branch. Above the claude branch on purpose: stop_claude's
    // no-transport-id fallback signals the recorded pid instead, which kills
    // the process inside the pane and leaves the pane itself.
    if let Some(mux) = entry.mux.as_ref() {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            format!(
                "agent {name} is a pane worker; `stop` reaches no pane and would report a \
                 stop it did not perform. Kill the pane: \
                 `fno mux pane kill {}:{}`. The registry row survives that; \
                 clear it with `fno agents rm {name}`.",
                mux.session, mux.pane_id
            ),
        );
    }
    // Claude agents are not PTY-managed (LD8): there is no worker to shut down.
    // Shell out to the claude supervisor and propagate its outcome.
    if entry.harness_name() == "claude" {
        return stop_claude(ctx, req, &name, &entry).await;
    }
    if is_codex_thread_entry(&entry) {
        // Stop means INTERRUPT the in-flight turn, then DROP the actor
        // (closing its connection to the shared daemon), and only then stamp
        // Exited. The old shape removed the handle and stamped Exited without
        // interrupting: a driving turn still held an Arc clone and the verb
        // reported a stop it did not perform.
        //
        // The interrupt IS the stop now. There is no child to kill: the
        // shared daemon owns the thread, so a turn that survives the bounded
        // settle keeps running there, and the report below says exactly that
        // rather than claiming a kill this verb cannot perform.
        let handle = ctx.codex_threads.lock().await.get(&name).cloned();
        let mut interrupt_report = "no-turn".to_string();
        // Did the turn actually reach a terminal state? With a private child,
        // `kill_on_drop` made every outcome terminal, so the answer was always
        // yes. Against the shared daemon an unconfirmed interrupt leaves the
        // turn RUNNING, and reporting a stop over it is the zombie shape this
        // arm exists to avoid.
        let mut settled = true;
        if let Some(handle) = handle.as_ref() {
            let outcome = match tokio::time::timeout(
                crate::codex_thread::stop_settle_bound(),
                handle.interrupt(),
            )
            .await
            {
                Ok(Ok(InterruptOutcome::NoTurnInFlight)) => "no-turn".to_string(),
                Ok(Ok(InterruptOutcome::Interrupted(receipt))) => receipt.status,
                Ok(Ok(InterruptOutcome::Timeout)) => {
                    settled = false;
                    "timeout-turn-still-running".to_string()
                }
                Ok(Err(error)) => {
                    settled = false;
                    format!("interrupt-failed-turn-still-running: {error}")
                }
                Err(_) => {
                    settled = false;
                    "interrupt-failed-turn-still-running: stop exchange timed out".to_string()
                }
            };
            interrupt_report = outcome;
            if settled {
                // Shutdown acks only after the driver dropped, so the daemon
                // connection is already closed before the row reads Exited.
                let _ = handle.shutdown().await;
            }
        }
        if !settled {
            // Keep the handle and leave the row non-terminal. The actor still
            // holds the interrupt handle for the live turn, so a retry can
            // reach it, and a terminal row would also make the thread
            // invisible to `codex_thread_recovery_candidate`.
            let _ = ctx.emitter.emit(
                "agent_stop_refused",
                &json!({"name": name, "backend": "codex-thread", "interrupt": interrupt_report}),
            );
            return Response::ok(
                req.id,
                json!({
                    "stopped": false,
                    "backend": "codex-thread",
                    "interrupt": interrupt_report,
                }),
            );
        }
        ctx.codex_threads.lock().await.remove(&name);
        let stop_name = name.clone();
        if let Err(error) = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
            if let Some(entry) = registry.find_mut(&stop_name) {
                entry.status = AgentStatus::Exited;
                entry.exited_at = Some(now_rfc3339_like());
            }
        })
        .await
        {
            return Response::err(
                req.id,
                state_error_code(&error),
                format!("codex thread {name} stopped but registry write failed: {error}"),
            );
        }
        let _ = ctx.emitter.emit(
            "agent_stopped",
            &json!({"name": name, "backend": "codex-thread", "interrupt": interrupt_report}),
        );
        return Response::ok(
            req.id,
            json!({
                "stopped": true,
                "backend": "codex-thread",
                "interrupt": interrupt_report,
            }),
        );
    }
    // A lane-B keeper thread (x-889a): fno's own keeper hosts the child and
    // the row's short_id is empty, so without this arm the no-op arm below
    // reports a stop that stopped nothing (PR 1332 review finding). Kill is
    // delivered over the row's own socket and CONFIRMED before the row goes
    // terminal; a keeper that will not die leaves the row non-terminal.
    if let Some(sock) = keeper_thread_sock(&entry) {
        if !stop_keeper_confirmed(&sock).await {
            let _ = ctx.emitter.emit(
                "agent_stop_refused",
                &json!({"name": name, "backend": "keeper-thread"}),
            );
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!("agent {name}: keeper did not confirm shutdown; it may still be running"),
            );
        }
        let stop_name = name.clone();
        if let Err(error) = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
            if let Some(entry) = registry.find_mut(&stop_name) {
                entry.status = AgentStatus::Exited;
                entry.exited_at = Some(now_rfc3339_like());
            }
        })
        .await
        {
            return Response::err(
                req.id,
                state_error_code(&error),
                format!("keeper thread {name} stopped but registry write failed: {error}"),
            );
        }
        let _ = ctx.emitter.emit(
            "agent_stopped",
            &json!({"name": name, "backend": "keeper-thread"}),
        );
        return Response::ok(req.id, json!({"stopped": true, "backend": "keeper-thread"}));
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
            &json!({"name": name, "provider": entry.harness_name(), "claude_exit": Value::Null}),
        );
        return Response::ok(
            req.id,
            json!({"stopped": true, "provider": entry.harness_name(), "no_op": true}),
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
            state_error_code(&e),
            format!("agent {name}: worker stopped but registry write failed: {e}"),
        );
    }
    let _ = ctx.emitter.emit("agent_stopped", &json!({"name": name}));
    Response::ok(req.id, json!({"stopped": true, "short_id": entry.short_id}))
}

/// Bound on a worker's shutdown ACK (x-3498 review): a wedged worker must not
/// hang the daemon's stop handler - the client above it would then report the
/// DAEMON as unresponsive and prescribe killing it, orphaning the very worker
/// being stopped. No ack inside this window reads as no ack; the caller's
/// SIGTERM -> SIGKILL escalation is the recovery.
const WORKER_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the WRITE half of a `worker.shutdown` round trip (x-76d1 self-review
/// finding, mirrors [`crate::client::WRITE_TIMEOUT`]). A small JSON request to an
/// already-connected local socket clears the kernel send buffer near instantly
/// unless the worker has stopped reading its socket entirely; kept short and
/// separate from `WORKER_ACK_TIMEOUT` so pairing it with the read's own 30s bound
/// does not silently double the documented shutdown-ack budget in
/// [`crate::client::RESPONSE_DEADLINE`]'s worst-case math.
const WORKER_ACK_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Fire-and-forget `worker.shutdown` to a worker that must not be left running
/// (a spawn that failed or lost a name race): connect, ask it to tear down, and
/// move on. Best-effort by design — the caller is already on an error path.
///
/// Same write+read bound as [`stop_worker_confirmed`]'s step 1 (<=35s worst
/// case), paid on the `agent.adopt_stream` spawn-error paths that call this.
/// Those paths return well before [`crate::client::RESPONSE_DEADLINE`], so
/// this worst case needs no separate line in that constant's budget comment.
async fn best_effort_worker_shutdown(sock: &std::path::Path) {
    if let Ok(mut conn) = UnixStream::connect(sock).await {
        let _ = tokio::time::timeout(
            WORKER_ACK_WRITE_TIMEOUT,
            write_request(&mut conn, &Request::new(1, "worker.shutdown", json!({}))),
        )
        .await;
        let _ = tokio::time::timeout(
            WORKER_ACK_TIMEOUT,
            crate::protocol::read_response(&mut conn),
        )
        .await;
    }
}

/// Graceful worker shutdown with SIGTERM -> SIGKILL escalation (US6.7), then
/// verify the worker process is actually gone. Returns true iff the worker is
/// confirmed down. A worker that never dies returns false so the caller can
/// refuse to claim a clean stop (a swallowed failure would mark the agent exited
/// while its PTY keeps running, Codex P1).
/// A lane-B keeper row's own socket: `messaging_socket_path` under
/// `mux/threads/`, the same predicate mail_inject's resolve_keeper_target_in
/// keys on. The keeper speaks the pane_keeper frame protocol, never worker
/// JSON-RPC, so it must not reach the `worker_sock` probe below - that probe
/// derives `worker_sock("")` from a lane-B row's empty short_id and would
/// confirm a stop over a socket the keeper does not own, orphaning keeper and
/// child (PR 1332 review finding).
fn keeper_thread_sock(entry: &RegistryEntry) -> Option<std::path::PathBuf> {
    let path = entry.messaging_socket_path.as_deref()?;
    path.contains("mux/threads/")
        .then(|| std::path::PathBuf::from(path))
}

/// Stop a lane-B keeper-hosted thread: one Kill frame over the row's own
/// socket, then the socket-unreachable confirmation every stop path answers
/// with. Only a seated subscriber's Kill is honored (pane_keeper.rs: first
/// come, first seated; the slot clears on disconnect), so a viewer holding
/// the seat makes this time out and the caller refuses rather than reporting
/// a stop it did not perform. On Kill the keeper SIGKILLs the child, unlinks
/// its socket and exits, so "down" here covers keeper AND child.
async fn stop_keeper_confirmed(sock: &std::path::Path) -> bool {
    use tokio::io::AsyncWriteExt;
    if let Ok(mut conn) = tokio::net::UnixStream::connect(sock).await {
        let frame = crate::pane_keeper::encode(&crate::pane_keeper::Frame::Kill);
        let _ = tokio::time::timeout(WORKER_ACK_WRITE_TIMEOUT, conn.write_all(&frame)).await;
    }
    let down = worker_down_within(sock, Duration::from_secs(5)).await;
    if down {
        // A SIGKILLed keeper cannot unlink its own socket; reap the stale
        // file only after the listener is confirmed gone (Codex P1 rule).
        let _ = std::fs::remove_file(sock);
    }
    down
}

async fn stop_worker_confirmed(ctx: &Ctx, entry: &RegistryEntry) -> bool {
    stop_worker_confirmed_for_home(&ctx.home, entry).await
}

/// The home-keyed body of [`stop_worker_confirmed`], shared with the
/// retirement sweep (x-c672), which holds an `AgentsHome` and no `Ctx`.
pub(crate) async fn stop_worker_confirmed_for_home(
    home: &AgentsHome,
    entry: &RegistryEntry,
) -> bool {
    // A lane-B keeper thread's lifecycle lives on its own socket (see
    // `keeper_thread_sock`); delegate before any worker_sock probe.
    if let Some(sock) = keeper_thread_sock(entry) {
        return stop_keeper_confirmed(&sock).await;
    }
    let sock = home.worker_sock(&entry.short_id);
    // 1. Graceful: ask the worker to tear down its PTY child + exit. Both the
    //    write (WORKER_ACK_WRITE_TIMEOUT) and the ACK read (WORKER_ACK_TIMEOUT)
    //    are bounded, asymmetrically like the client's own request/response
    //    split: a worker that is wedged (including one that has stopped
    //    reading its socket entirely, which blocks the write side too) must
    //    fall through to the escalation below, not park this handler.
    if let Ok(mut conn) = UnixStream::connect(&sock).await {
        let _ = tokio::time::timeout(
            WORKER_ACK_WRITE_TIMEOUT,
            write_request(&mut conn, &Request::new(1, "worker.shutdown", json!({}))),
        )
        .await;
        let _ = tokio::time::timeout(
            WORKER_ACK_TIMEOUT,
            crate::protocol::read_response(&mut conn),
        )
        .await;
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
/// Whether `pid` is confirmed GONE, as opposed to merely unreachable.
///
/// `pid_is_ours` answers "may I treat this as my worker", and returns false for
/// two very different reasons: the process is dead (ESRCH), or it is alive but
/// unsignalable (EPERM) / recycled. Using it as a death oracle turns "I cannot
/// tell" into "it stopped", which reports a clean stop over a process that is
/// still running. Only ESRCH is death.
fn pid_confirmed_dead(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        // Never signalled in the first place, so nothing is running on our behalf.
        return true;
    }
    // SAFETY: signal 0 is an existence/permission probe only, no signal is sent.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return false; // reachable => alive
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Whether `pid` is PROVABLY a different incarnation than the one recorded.
///
/// Not the negation of `pid_is_ours`. That returns false for three different
/// situations -- dead, recycled, and alive-but-unsignalable (EPERM) -- so using
/// it as a recycle test folds EPERM back into "gone" and reinstates the very
/// clean-stop-over-a-live-process bug `pid_confirmed_dead` exists to prevent.
/// A recycle claim needs positive evidence: the pid is reachable AND its start
/// token is readable AND it differs from the recorded one. Anything less is no
/// verdict, which leaves the caller waiting and, ultimately, reporting failure.
fn pid_recycled(pid: u32, recorded_start: Option<u64>) -> bool {
    let Some(recorded) = recorded_start else {
        return false; // nothing to compare against
    };
    if pid <= 1 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe only, no signal is sent.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false; // dead or unsignalable: not a positive recycle finding
    }
    match process_start_time(pid) {
        Some(now) => now != recorded,
        None => false, // unreadable: no verdict
    }
}

/// Poll until `pid` is confirmed dead, or `budget` elapses.
///
/// A pid that got RECYCLED mid-wait also ends the wait: the process we signalled
/// is gone, which is what the caller asked about, and the new occupant is not
/// ours to keep waiting on. Both arms demand positive evidence, so an
/// alive-but-unsignalable process satisfies neither and the wait runs out --
/// reporting failure, which is the honest answer when we cannot see.
async fn pid_gone_within(pid: u32, recorded_start: Option<u64>, budget: Duration) -> bool {
    let start = Instant::now();
    loop {
        if pid_confirmed_dead(pid) || pid_recycled(pid, recorded_start) {
            return true;
        }
        if start.elapsed() >= budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stop a claude row that has a recorded pid but no transport id, with the same
/// SIGTERM -> SIGKILL escalation `stop_worker_confirmed` uses. Returns true iff
/// the process is confirmed gone.
///
/// A row can carry a live process and no short id at all when the spawn receipt
/// never yielded one. Refusing there left the operator with a running worker and
/// no verb that addressed it -- the duplicate-worker half of the wave-boundary
/// handoff failure, which had to be killed by hand to restore one-writer
/// semantics. Unlike a PTY worker there is no socket to probe, so `pid_is_ours`
/// (which rejects pid <= 1, treats an unsignalable pid as not ours, and compares
/// the recorded start time) is both the liveness oracle and the recycle guard.
/// It is re-proved before EVERY signal so a pid recycled inside the grace window
/// is never killed.
async fn stop_claude_pid_confirmed(entry: &RegistryEntry) -> bool {
    let Some(pid) = entry.pid else {
        return false;
    };
    // Require the incarnation token. Without it `pid_is_ours` falls back to bare
    // liveness, which cannot tell our worker from an unrelated process that
    // inherited the pid after it died. That is tolerable for a probe; it is not
    // tolerable as the sole basis for SIGKILL. Refusing costs a legacy row an
    // honest "cannot stop" message. Guessing costs someone else's process.
    if entry.pid_start_time.is_none() {
        return false;
    }
    if !pid_is_ours(pid, entry.pid_start_time) {
        return false;
    }
    // SAFETY: pid ownership proved directly above; SIGTERM to our own worker.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if pid_gone_within(pid, entry.pid_start_time, Duration::from_secs(5)).await {
        return true;
    }
    if pid_is_ours(pid, entry.pid_start_time) {
        // SAFETY: ownership re-proved after the grace window, so a pid recycled
        // during it takes no signal.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    pid_gone_within(pid, entry.pid_start_time, Duration::from_secs(2)).await
}

async fn stop_claude(ctx: &Ctx, req: &Request, name: &str, entry: &RegistryEntry) -> Response {
    let short = match entry
        .transport_short()
        .or(entry.session_id.as_deref())
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_string(),
        None => {
            // No transport id: fall back to signalling the recorded pid rather
            // than refusing a row whose process is still running.
            if stop_claude_pid_confirmed(entry).await {
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
                        state_error_code(&e),
                        format!("claude {name} stopped but registry write failed: {e}"),
                    );
                }
                let _ = ctx.emitter.emit(
                    "agent_stopped",
                    &json!({"name": name, "backend": "claude", "stopped_by": "pid"}),
                );
                return Response::ok(
                    req.id,
                    json!({"stopped": true, "backend": "claude", "pid": entry.pid}),
                );
            }
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "agent {name} is claude but has no short id and no live process \
                     to stop. `rm` will refuse this row too while it is stored live, so \
                     stop-then-rm has no exit here: the row can neither prove liveness \
                     nor be addressed. The override for that case is documented in \
                     `fno agents rm --help`, not here."
                ),
            );
        }
    };
    // Bound the subprocess so a hung `claude` can never wedge this RPC
    // handler, the same way the background-sweep twin above is bounded.
    match bounded_claude_stop(&short, Duration::from_secs(15)).await {
        Err(_) => Response::err(
            req.id,
            ErrorCode::Internal,
            // retired-ok: reports which shellout timed out, not a step to run.
            format!("claude stop {short} timed out"),
        ),
        Ok(Ok(o)) if o.status.success() => {
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
                    state_error_code(&e),
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
        Ok(Ok(o)) => Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                // retired-ok: reports which shellout failed, not a step to run.
                "claude stop {short} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        ),
        Ok(Err(e)) => Response::err(
            req.id,
            ErrorCode::Internal,
            format!("could not exec `claude stop`: {e}"),
        ),
    }
}

fn run_mux_pane_kill(session: &str, pane_id: u64) -> Result<bool, String> {
    let pane_id = pane_id.to_string();
    let mut child = std::process::Command::new("fno")
        .args(["mux", "pane", "kill", "--session", session, &pane_id])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("mux pane kill failed to start: {error}"))?;
    let deadline = std::time::Instant::now() + CASCADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(true),
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                let output = child.wait_with_output().ok();
                let detail = output
                    .as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stderr).to_ascii_lowercase())
                    .unwrap_or_default();
                if mux_pane_is_absent(&detail) {
                    return Ok(false);
                }
                return Err(format!("mux pane kill exited {code}: {}", detail.trim()));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("mux pane kill timed out".into());
            }
            Err(error) => return Err(format!("mux pane kill wait failed: {error}")),
        }
    }
}

fn mux_pane_is_absent(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("no such pane")
        || detail.contains("no live pane owns")
        || (detail.contains("cannot reach session")
            && (detail.contains("no such file or directory")
                || detail.contains("connection refused")))
}

/// What a read-only look at the pane referent proved. `Unknown` is the
/// fail-closed posture: a probe that cannot prove absence changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneProbe {
    Present,
    Absent,
    Unknown,
}

/// Probe whether the pane a registry row's mux ref names still exists, without
/// touching it: a one-line `pane read` against that session. The absence
/// vocabulary is the same `mux_pane_is_absent` set the kill cascade trusts, so
/// "absent" means the mux layer itself said the pane is gone.
fn run_mux_pane_probe(session: &str, pane_id: u64) -> PaneProbe {
    let pane = pane_id.to_string();
    let mut child = match std::process::Command::new("fno")
        .args([
            "mux",
            "pane",
            "read",
            "--session",
            session,
            "--lines",
            "1",
            &pane,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return PaneProbe::Unknown,
    };
    let deadline = std::time::Instant::now() + CASCADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                if exit.success() {
                    return PaneProbe::Present;
                }
                let output = child.wait_with_output().ok();
                let detail = output
                    .as_ref()
                    .map(|o| {
                        let mut text = String::from_utf8_lossy(&o.stderr).to_ascii_lowercase();
                        text.push_str(&String::from_utf8_lossy(&o.stdout).to_ascii_lowercase());
                        text
                    })
                    .unwrap_or_default();
                if mux_pane_is_absent(&detail) {
                    return PaneProbe::Absent;
                }
                return PaneProbe::Unknown;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return PaneProbe::Unknown;
            }
            Err(_) => return PaneProbe::Unknown,
        }
    }
}

/// A row whose ONE live ref is a mux pane is as live as that pane: the stored
/// enum only records what fno last wrote, so the gate must test the referent.
/// Proof of absence comes only from the probe's `Absent` verdict.
fn pane_provably_absent(
    mux: Option<&state::MuxRef>,
    probe: &(dyn Fn(&str, u64) -> PaneProbe + Sync),
) -> bool {
    match mux {
        Some(mux) => probe(&mux.session, mux.pane_id) == PaneProbe::Absent,
        None => false,
    }
}

async fn handle_rm(ctx: &Ctx, req: &Request) -> Response {
    handle_rm_with(
        ctx,
        req,
        &crate::claude_roster::read_all_agents,
        &run_claude_rm,
        &run_mux_pane_kill,
        &run_mux_pane_probe,
    )
    .await
}

fn cleanup_king_manifest(entry: &state::RegistryEntry) {
    let Some(scope) = entry.crown_scope.as_deref() else {
        return;
    };
    if scope.is_empty()
        || scope.contains("..")
        || scope.contains('/')
        || scope.contains('\\')
        || scope.contains('\0')
    {
        return;
    }
    let path = std::path::Path::new(&entry.cwd)
        .join(".fno")
        .join("kings")
        .join(format!("{scope}.md"));
    // Owner guard, the Rust half of Python remove_king_manifest's
    // expected_harness_session_id: a successor crowned over this scope after
    // the row went terminal can have re-armed the manifest with ITS session
    // id, and deleting unconditionally would disarm that live king. Skip only
    // on a PROVEN foreign owner (the manifest names a different session id);
    // an id-less or matching manifest deletes on the registry's own authority,
    // which is what rm acts on. The cwd join stays entry-relative: a
    // subdirectory cwd may miss the repo-root manifest and leave a stale
    // file, which is the same safe direction.
    if let Ok(content) = std::fs::read_to_string(&path) {
        let current = content
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("harness_session_id:"))
            .map(|v| v.trim().trim_matches('"').to_string());
        let expected = entry
            .harness_session_id
            .as_deref()
            .filter(|s| !s.is_empty());
        if let (Some(exp), Some(cur)) = (expected, current) {
            if cur != exp {
                return;
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

async fn handle_rm_with(
    ctx: &Ctx,
    req: &Request,
    read_claude_agents: &(dyn Fn() -> crate::claude_roster::ClaudeAgentsSnapshot + Sync),
    claude_rm: &(dyn Fn(&str) -> Result<(), String> + Sync),
    mux_pane_kill: &(dyn Fn(&str, u64) -> Result<bool, String> + Sync),
    mux_pane_probe: &(dyn Fn(&str, u64) -> PaneProbe + Sync),
) -> Response {
    let requested_name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `name`"),
    };
    let force = req
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
    let entry =
        match entry_for_lifecycle(&registry, &requested_name, &ctx.home.registry_json()).await {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return Response::err(
                    req.id,
                    ErrorCode::AgentNotFound,
                    format!("agent {requested_name} not found"),
                )
            }
            Err(message) => return Response::err(req.id, ErrorCode::InvalidParams, message),
        };
    let name = entry.name.clone();
    let audit = RemovalAuditContext::from_request(req, &entry);
    // Computed once (self-review finding): every other reference in this
    // handler reuses this allocation instead of re-deriving the same short id.
    let harness_row_id = claude_row_id(&entry);
    let claude_agents = if entry.harness_name() == "claude" {
        Some(read_claude_agents())
    } else {
        None
    };
    if claude_agents
        .as_ref()
        .and_then(|snapshot| harness_row_id.as_deref().and_then(|id| snapshot.find(id)))
        .and_then(|row| row.state.as_deref())
        == Some("blocked")
    {
        return Response::err(
            req.id,
            ErrorCode::Busy,
            format!(
                "agent {name} is blocked (model outage); rotate it to another model rather than reaping it."
            ),
        );
    }
    // The stored enum is what fno last WROTE, not what is true: a session torn
    // down by hand never updates it. Two truths prove the row gone, each with
    // its own fail-closed posture. A claude row absent from the `claude agents
    // --json --all` roster is provably gone, whoever removed it (claude-only;
    // `claude_row_provably_absent` is unconditionally false elsewhere). A pane
    // row whose terminal state is explicit is finished even though Claude
    // keeps it in the roster, and a pane row whose pane the probe cannot find
    // is provably gone because the pane is that row's ONE live ref. Anything
    // less than proof keeps refusing, and `--force` remains the only escape.
    let row_state_terminal = claude_agents
        .as_ref()
        .and_then(|snapshot| harness_row_id.as_deref().and_then(|id| snapshot.find(id)))
        .and_then(|row| row.state.as_deref())
        .is_some_and(|state| matches!(state, "done" | "stopped" | "failed"));
    let provably_gone = row_state_terminal
        || claude_row_provably_absent(claude_agents.as_ref(), harness_row_id.as_deref())
        || pane_provably_absent(entry.mux.as_ref(), mux_pane_probe);
    if entry.status == AgentStatus::Live && !force && !provably_gone {
        let row = harness_row_id
            .clone()
            .unwrap_or_else(|| "(no harness row id)".into());
        let roster_known = claude_agents.as_ref().is_some_and(|snap| snap.is_known());
        let detail = if entry.harness_name() != "claude" {
            format!(
                "agent {name} is still live. Stop it with `fno agents stop {name}`; rm \
                 proceeds on its own once the row is gone. Forcing it through orphans a \
                 live process and spends the row's resume handle. If stop answers no_op \
                 (no addressable session behind the row), the row cannot prove liveness \
                 either way; the override for that case is documented in `fno agents rm \
                 --help`, not here."
            )
        } else if harness_row_id.is_none() {
            // claude_row_provably_absent short-circuits to `false` (not
            // provably gone) whenever the row id is None, independent of the
            // roster -- so presence was never actually checked here, and the
            // roster_known branch's "is present in" claim plus its runnable
            // `claude stop <row>` commands would both be false (self-review
            // finding).
            format!(
                "agent {name} is still live, but it has no resolvable harness row id, so \
                 its presence in `claude agents --json --all` cannot be checked. Stop it \
                 with `fno agents stop {name}`; rm proceeds on its own once the row is \
                 gone. Forcing it through spends the resume handle the row still holds. \
                 If stop refuses or no-ops because the row has no addressable session, \
                 the row cannot prove liveness either way; the override for that case is \
                 documented in `fno agents rm --help`, not here."
            )
        } else if roster_known {
            format!(
                "agent {name} is still live. Its harness row {row} is present in \
                 `claude agents --json --all`. Stop it with `fno agents stop {name}`; rm \
                 proceeds on its own once that row is gone. Do not tear the row down by \
                 hand: that spends the resume handle for nothing, and `fno agents rm` \
                 makes the same call itself."
            )
        } else {
            format!(
                "agent {name} is still live, and its harness row {row}'s presence in \
                 `claude agents --json --all` could not be confirmed (the roster read \
                 failed). Retry once the roster is readable: rm re-reads it and proceeds \
                 on its own when the row is provably gone. Forcing it through spends the \
                 resume handle on unverified evidence."
            )
        };
        return Response::err(req.id, ErrorCode::Busy, detail);
    }
    let harness_outcome = cascade_harness_session_result_with(
        &entry,
        claude_agents.as_ref(),
        read_claude_agents,
        claude_rm,
    );
    if let CascadeOutcome::Failed(reason) = &harness_outcome {
        if !force {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!("agent {name}: harness removal failed: {reason}"),
            );
        }
    }
    let pane_outcome = if let Some(mux) = entry.mux.as_ref() {
        match mux_pane_kill(&mux.session, mux.pane_id) {
            Ok(true) => CascadeOutcome::Removed,
            Ok(false) => CascadeOutcome::AlreadyAbsent("mux pane already absent".into()),
            Err(reason) => CascadeOutcome::Failed(reason),
        }
    } else {
        CascadeOutcome::NotApplicable
    };
    if let CascadeOutcome::Failed(reason) = &pane_outcome {
        if !force {
            let harness_note = match &harness_outcome {
                CascadeOutcome::Removed => format!(
                    "{} harness row {} removed; ",
                    entry.harness_name(),
                    harness_row_id.as_deref().unwrap_or("unknown")
                ),
                CascadeOutcome::AlreadyAbsent(_) => "harness row already absent; ".into(),
                _ => String::new(),
            };
            let mux = entry.mux.as_ref().expect("pane outcome requires a mux ref");
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!(
                    "agent {name}: {harness_note}registry retained; mux pane {}:{} removal failed: {reason}",
                    mux.session, mux.pane_id
                ),
            );
        }
    }
    // Removing a live agent must stop its own worker first, or it leaks a PTY
    // process that `list`/`stop` can no longer address by name (Codex P2). The
    // `provably_gone` path proceeds without `--force`, but it only proves the
    // HARNESS session is gone; this row's local worker.sock is a separate
    // process and can still be alive, so it needs the same confirmation.
    if entry.status == AgentStatus::Live
        && (force || provably_gone)
        && !stop_worker_confirmed(ctx, &entry).await
    {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("agent {name}: could not stop the worker before removing a live row; refusing to orphan a live PTY"),
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
    // Identity, not just name: `entry` was resolved off-lock, so a respawn
    // under the same name between resolution and this write is a different
    // row. Matching name alone would silently drop the NEW (possibly live)
    // row instead of the one this request actually resolved and tore down --
    // the same race `dispatch.py`'s `_recipient_identity_key` guards against,
    // and `row_identity_matches` (this file, shared with
    // `switchboard_identity_matches`) guards for mail delivery. `created_at`
    // is load-bearing, not decorative: a codex row's `harness_session_id`
    // sits at `None` until `late_bind_codex_sessions` binds it, and
    // `short_id` is deterministically derived from `name`, so a respawn
    // under a just-freed name can otherwise reproduce every other field on
    // the stale row while it waits on its own late-bind.
    let rm_short_id = entry.short_id.clone();
    let rm_session_id = entry.harness_session_id.clone();
    let rm_created_at = entry.created_at.clone();
    // retain() cannot fail and cannot report what it dropped, so count across
    // it via the closure's return value: a resolved name that no row in the
    // file actually carries must not report removed:true (the silent no-op
    // mode). Nor can it report WHICH rows it dropped, so the identity match
    // above is also the only defense against dropping more than the one row
    // this request resolved -- checked below.
    let dropped = match update_registry_offloaded(ctx.home.registry_json(), move |r| {
        let before = r.entries.len();
        r.entries.retain(|e| {
            !row_identity_matches(
                e,
                &RowIdentity {
                    harness: None,
                    name: Some(&rm_name),
                    short_id: &rm_short_id,
                    session_id: rm_session_id.as_deref(),
                    created_at: &rm_created_at,
                },
            )
        });
        before - r.entries.len()
    })
    .await
    {
        Ok(dropped) => dropped,
        Err(e) => {
            return Response::err(
                req.id,
                state_error_code(&e),
                format!("agent {name}: removal did not persist: {e}"),
            );
        }
    };
    if dropped == 0 {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                "agent {name}: resolved to a row the registry does not hold; nothing was removed. \
                 Re-read it with `fno agents list --json` and rm by the exact `name` field."
            ),
        );
    }
    if dropped > 1 {
        // The identity match above should select at most one row; more than
        // one is an invariant violation, not a normal outcome, and must be
        // loud rather than reported as a clean single-row removal.
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                "agent {name}: identity match dropped {dropped} rows, expected at most 1; \
                 registry may need manual repair"
            ),
        );
    }
    cleanup_king_manifest(&entry);
    // (x-d545) The row is gone from the registry: take its worktree, but only
    // as far as the reapable gate allows. The receipt rides the RESULT (the
    // operator's notice), deliberately NOT the event: agent_removed sits
    // near the 500-byte event cap already, so the receipt would push every
    // rm event over it and the writer would replace the whole record. The
    // auditable event field is x-90ee's to land with a shape that fits.
    let worktree_path = std::path::Path::new(&entry.cwd);
    let detected_worktree = is_linked_worktree(&entry.cwd);
    let worktree_touched = audit.worktree_touched.unwrap_or(detected_worktree);
    let measured_bytes = if detected_worktree {
        directory_bytes(worktree_path)
    } else {
        None
    };
    let worktree_receipt = rm_take_worktree(&entry);
    let worktree_removed = worktree_touched && !worktree_path.exists();
    let reclaimed_bytes = audit.reclaimed_bytes.unwrap_or_else(|| {
        if worktree_removed {
            measured_bytes.unwrap_or(0)
        } else {
            0
        }
    });
    let worktree_outcome = if !worktree_touched {
        "not-touched"
    } else if worktree_removed {
        "removed"
    } else {
        "kept"
    };
    let pane_session = entry.mux.as_ref().map(|mux| mux.session.clone());
    let pane_id = entry.mux.as_ref().map(|mux| mux.pane_id);
    let event = json!({
        "name": name,
        "registry_changed": true,
        "harness": entry.harness_name(),
        "harness_session_id": entry.harness_session_id,
        "actor": audit.actor,
        "reason": audit.reason,
        "request_id": audit.request_id,
        "worktree_touched": worktree_touched,
        "worktree_outcome": worktree_outcome,
        "reclaimed_bytes": reclaimed_bytes,
        "harness_removed": harness_outcome.removed_json(),
        "pane_removed": pane_outcome.removed_json(),
    });
    let event_payload_len = serde_json::to_string(&event)
        .map(|encoded| encoded.len())
        .unwrap_or(usize::MAX);
    let event_error = match ctx.emitter.emit("agent_removed", &event) {
        Err(error) => Some(error.to_string()),
        Ok(()) if event_payload_len > crate::events::MAX_EVENT_PAYLOAD_BYTES => Some(format!(
            "agent_removed event replaced by event_payload_too_large ({event_payload_len} bytes)"
        )),
        Ok(()) => None,
    };
    let result = json!({
        "removed": true,
        "registry_removed": true,
        "registry_changed": true,
        "harness": entry.harness_name(),
        "harness_row_id": harness_row_id,
        // The FULL session id, distinct from harness_row_id above: that one
        // falls back to the first eight chars of this, which is not a valid
        // adopt key for a codex row (time-prefixed ids collide across
        // same-window sessions) and is not even hex for a non-uuid id. The
        // client's adopt hint needs a handle that resolves uniquely.
        "harness_session_id": entry.harness_session_id,
        "harness_removed": harness_outcome.removed_json(),
        "harness_reason": harness_outcome.reason(),
        "pane_session": pane_session,
        "pane_id": pane_id,
        "pane_removed": pane_outcome.removed_json(),
        "pane_reason": pane_outcome.reason(),
        "worktree_receipt": worktree_receipt,
        "actor": audit.actor,
        "reason": audit.reason,
        "request_id": audit.request_id,
        "worktree_touched": worktree_touched,
        "worktree_outcome": worktree_outcome,
        "reclaimed_bytes": reclaimed_bytes,
        "event_written": event_error.is_none(),
        "event_reason": event_error,
        "was_orphaned": was_orphaned,
    });
    Response::ok(req.id, result)
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
    /// The probe's liveness word, `alive|dead|unmeasured`, decided
    /// where the evidence was gathered and written beside
    /// `liveness_measured_at`. `None` = not measured this sweep (deferred or
    /// no evidence): leave the previous measurement standing, its age honest
    /// on the wire.
    new_liveness: Option<&'static str>,
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
/// - Ask-bucket rows (one-shot asks AND claude bg threads, x-5d96): a roster
///   `bg_live` hit plus a SILENT liveness ladder (no socket, no advancing
///   heartbeat, no working truth state) transitions `Orphaned` - the
///   reversible state - so roster presence can no longer pin a zombie row
///   `live` forever. An `Alive` ladder answer blocks the flip.
///
/// `liveness` is the shared reader (x-5d96), injected like `probe` so the
/// ladder is deterministically stageable in tests.
#[allow(clippy::too_many_arguments)]
fn plan_reconcile<P, D, L, B, H, R, V>(
    entries: &[RegistryEntry],
    mut probe: P,
    mut budget_exhausted: D,
    mut pid_live: L,
    mut bg_live: B,
    mut thread_hosted: H,
    mut rollout_exists: R,
    mut liveness: V,
    roster_readable: bool,
) -> (Vec<ReconcileChange>, ReconcileOutcome)
where
    P: FnMut(&RegistryEntry) -> Result<bool, crate::provider::ReachabilityProbeError>,
    D: FnMut() -> bool,
    L: FnMut(&RegistryEntry) -> bool,
    B: FnMut(&RegistryEntry) -> bool,
    H: FnMut(&RegistryEntry) -> bool,
    R: FnMut(&RegistryEntry) -> bool,
    V: FnMut(&RegistryEntry) -> RowLiveness,
{
    let mut changes = Vec::new();
    let mut out = ReconcileOutcome::default();
    for (i, entry) in entries.iter().enumerate() {
        if budget_exhausted() {
            out.deferred = entries.len() - i;
            break;
        }
        // A Codex thread hosted by THIS daemon is owned by its actor: the
        // stale registry pid must not settle it. A row no longer hosted is
        // settled by its rollout: the rollout file on disk is the durable
        // object, so its presence means Orphaned (resumable later, by a human
        // or a resume verb), its absence means the thread never got far enough
        // to persist anything and is Exited. Before the actor rewrite this arm
        // always returned None, so a permanently dead thread read Live forever.
        if is_codex_thread_entry(entry) {
            let new_status = if thread_hosted(entry) {
                None
            } else if rollout_exists(entry) {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Orphaned)
            } else {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Exited)
            };
            changes.push(ReconcileChange {
                name: entry.name.clone(),
                new_status,
                // Hosted = the actor answers for it: alive. A rollout means
                // resumable, not running; nothing on disk is gone. `None`
                // (hosted) keeps the previous measurement standing.
                new_liveness: match new_status {
                    Some(AgentStatus::Exited) | Some(AgentStatus::Orphaned) => Some("dead"),
                    _ => None,
                },
            });
            continue;
        }
        // A one-shot `ask` agent has no daemon-managed process, so its liveness is
        // decided by process-liveness alone (it has none): terminal `exited`.
        // Session-file reachability answers "resumable?" (surfaced via session_id),
        // never "running?" -- so a surviving session file must NOT keep an ask row
        // `live`. This is the actual cause of the reported stale-`live` rows: the
        // `probe` is skipped entirely here, so no provider reachability call can
        // decide an ask row's status. An already-terminal ask is left untouched.
        // [plan ab-70faa65b, Locked Decision #1]
        // A `claude --substrate bg` thread lands in this same bucket (claude
        // harness, no footnote pid, no mux) and yet it IS a running process --
        // claude's own daemon owns it and lists it in `roster.json`. Reaping it
        // unprobed made `wait --state done` answer "done (via exit)" seconds
        // after spawn, for a worker whose transcript was still growing, so a
        // court king read a live teammate as dead and could respawn a duplicate
        // against it. `bg_live` asks the roster before we declare death; a
        // genuinely finished ask is absent from it and still reaps to Exited.
        if entry.is_one_shot_ask() {
            let new_status = if is_non_terminal(entry.status) && !bg_live(entry) {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Exited)
            } else if matches!(
                entry.status,
                AgentStatus::Live | AgentStatus::Ready | AgentStatus::Idle | AgentStatus::Busy
            ) && roster_readable
                && bg_live(entry)
                && liveness(entry) == RowLiveness::Unknown
            {
                // x-5d96: a roster entry used to hold a claude row `live`
                // forever. Roster presence is weak evidence - a dead
                // supervisor can leave stale entries - so a row the shared
                // ladder answers `Unknown` on (no socket, no advancing
                // heartbeat, no working truth state) carries no positive
                // running-marker and goes `Orphaned` - never `Exited`,
                // silence never proves death. The flip needs a roster read
                // that SUCCEEDED: an unreadable roster is unknown liveness,
                // and orphaning a live worker on a transient instrumentation
                // failure is the false positive this arm must not produce.
                // Spawning is EXCLUDED: a row still coming up has had no
                // chance to produce any marker, so its silence is
                // meaningless (the same never-reap-something-still-coming-up
                // rule the sweep uses). An advancing heartbeat or a working
                // truth state answers Alive and blocks the flip (the x-d3ad
                // resurrected session). An Orphaned row is not re-visited
                // here (not live-ish), and gc still protects it: removal
                // needs positive corroboration, so a falsely-flipped live
                // worker keeps its transcript evidence and is never reaped.
                out.orphans.push(entry.name.clone());
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Orphaned)
            } else {
                None
            };
            changes.push(ReconcileChange {
                name: entry.name.clone(),
                new_status,
                // The ask arm's evidence, not a guess: a bg-live roster hit
                // with a silent ladder never positively answers, so it reads
                // unmeasured, never dead; a finished ask is gone.
                new_liveness: match new_status {
                    Some(AgentStatus::Exited) => Some("dead"),
                    Some(AgentStatus::Orphaned) => Some("unmeasured"),
                    _ => None,
                },
            });
            continue;
        }
        // One probe, two verdicts: the status transition (below) and the
        // SERVED liveness word both come from the same measurement,
        // so the wire can never claim an age or a word the sweep did not
        // itself just observe.
        let measured = probe(entry);
        let new_status = match &measured {
            Ok(true) => {
                // Recovery needs BOTH signals. A store hit alone means "the
                // session still exists" (= resumable), which for a store that
                // never evicts is permanently true - opencode's session table
                // keeps a row forever, so a dead pane would be resurrected to
                // `live` on every sweep and discovery would hand out a
                // recipient nobody drains. A row with no recorded pid keeps the
                // old behavior (`pid_live` is true), so exec rows are untouched.
                // Ask-bucket rows never reach this arm (they continue above),
                // so an Orphaned x-5d96 zombie cannot recover here and
                // oscillate: gc ages it from the terminal set instead.
                if entry.status == AgentStatus::Orphaned && pid_live(entry) {
                    out.recovered.push(entry.name.clone());
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Live)
                } else {
                    None
                }
            }
            Ok(false) if entry.is_interactive() => {
                // host_mode=interactive (task 2.3 / US4): a daemon-managed
                // interactive host is always pid'd; its liveness is the PTY
                // process, not the session store, so a store miss must not orphan
                // it. A dead worker reaps to Exited ("unexpected exit is exited,
                // not orphaned"; Codex P2, PR #373).
                if pid_live(entry) {
                    None
                } else {
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Exited)
                }
            }
            Ok(false) if entry.mux.is_some() => {
                // A mux-pane row is PTY-governed only with a captured pid. Mux
                // rows are written with the default exec host_mode but carry a mux
                // ref; without this arm, 1.1's backfilled codex id (or a claude
                // pane's minted id) would false-orphan a live pane on a store
                // miss. But pid_live maps None to true, so a pid-less mux row
                // (_lookup_child_pid best-effort miss) must NOT be preserved here
                // or a maybe-dead pane stays immortal -- it defers to store
                // liveness (orphan) instead. A live pid keeps it Live; a dead pid
                // reaps to Exited (Codex P1/P2, #603 r3/r4).
                if entry.pid.is_some() && pid_live(entry) {
                    None
                } else if entry.pid.is_some() {
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Exited)
                } else {
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
            new_liveness: match measured {
                Ok(true) => Some("alive"),
                Ok(false) => Some("dead"),
                Err(_) => Some("unmeasured"),
            },
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
/// The `Exited` transition also stamps `exited_at`: `last_reconciled_at` rotates
/// on every probe, so it is a CHECKED stamp, not a transition stamp, and the only
/// timestamp a reader can attribute to the exit itself is one written here.
fn apply_reconcile_change(
    e: &mut RegistryEntry,
    new_status: Option<AgentStatus>,
    new_liveness: Option<&str>,
    now: &str,
) {
    e.last_reconciled_at = Some(now.to_string());
    if let Some(word) = new_liveness {
        // The sweep is the ONLY writer of the served pair: a probe
        // answer is a fact about the moment it measured, so it carries its
        // stamp with it.
        e.liveness = Some(word.to_string());
        e.liveness_measured_at = Some(now.to_string());
    }
    if let Some(s) = new_status {
        e.status = s;
        if matches!(s, AgentStatus::Exited) {
            e.pid = None;
            e.pid_start_time = None;
            e.exited_at = Some(now.to_string());
            // Ordered exit teardown (E3.3, AC-X2-4): clear the inside-leg
            // authority on exit so a stale `working` never wins after the pane
            // is gone. The completion event is published by the caller BEFORE
            // this write (publish completion -> clear authority). A scraped
            // verdict dies with the pane for the same reason.
            e.inside_leg = None;
            e.screen_state = None;
        }
        if matches!(s, AgentStatus::Orphaned) {
            // x-5d96 (codex P2, PR 1329): the transition just re-decided the
            // row's liveness from current evidence, so any `exited_at` it
            // carried is a stamp from an earlier, falsified reading. Keeping
            // it would let gc age the row on a clock that started before the
            // re-decision and skip the grace window at its first real
            // dead-observation. Cleared, gc stamps fresh.
            e.exited_at = None;
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

/// The lowercase wire label for an inside-leg state. Allocation-free; the
/// single source for the three daemon-emitted inside-leg events.
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
    let session_id = match e.harness_name() {
        "codex" => e.codex_session_id.clone().or_else(|| e.session_id.clone()),
        "gemini" => e.gemini_session_id.clone().or_else(|| e.session_id.clone()),
        "claude" => e
            .transport_short()
            .map(str::to_string)
            .or_else(|| e.session_id.clone()),
        // Python writes opencode ids to the canonical harness_session_id and
        // drops `session_id` on write (it is Rust-set only), so falling through
        // to `session_id` would hand the probe None for every pane row and make
        // it a permanent no-op.
        "opencode" => e
            .harness_session_id
            .clone()
            .or_else(|| e.session_id.clone()),
        _ => e.session_id.clone(),
    };
    crate::provider::AgentEntry {
        name: e.name.clone(),
        provider: e.harness_name().to_string(),
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
/// Late bind (x-9de7 task 2): resolve a pane-hosted codex row's session id on
/// the reconcile tick, keyed on the PANE, not on cwd. `(harness, cwd)` is not
/// a join key -- 43 of 49 registry rows share a `(harness, cwd)` bucket with
/// a sibling on this machine, so joining on it would light every sibling
/// alive off one live transcript. The pane-tree rollout probe already used at
/// spawn time (`_codex_session_id_for_pid`) identifies a session down to the
/// exact pane, because each pane's process tree holds a distinct rollout.
///
/// A codex spawn's 8-second bind window (`_BINDING_WINDOW_S`) is real and is
/// NOT widened here: widening blocks the spawn caller longer, still loses the
/// race whenever codex is slower than whatever number is picked, and does
/// nothing for rows already on disk. This runs the same probe later instead,
/// bounded to rows that still need it (a live pid, a mux ref, no session id
/// yet -- a handful of rows, never the full registry), and NEVER from a
/// render path: `fno agents list --json` already shells one Python
/// subprocess per row and is not getting a second.
///
/// `probe` is injected so this is testable without shelling out.
fn predecessor_reachability(session_id: &str) -> Option<bool> {
    crate::truth_probe::family1_truth_probe(session_id).and_then(|probe| {
        match probe.reachability.as_deref() {
            Some("reachable") => Some(true),
            Some("unreachable") => Some(false),
            _ => None,
        }
    })
}

fn late_bind_codex_sessions(
    home: &AgentsHome,
    emitter: &EventEmitter,
    probe: &dyn Fn(u32) -> Option<String>,
) -> Result<(), String> {
    late_bind_codex_sessions_with_transition(home, emitter, probe, &predecessor_reachability)
}

fn late_bind_codex_sessions_with_transition(
    home: &AgentsHome,
    emitter: &EventEmitter,
    probe: &dyn Fn(u32) -> Option<String>,
    transition_probe: &dyn Fn(&str) -> Option<bool>,
) -> Result<(), String> {
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();
    let candidates: Vec<(String, u32, Option<String>)> = registry
        .entries
        .iter()
        .filter(|e| {
            e.harness_name() == "codex"
                && e.mux.is_some()
                && e.pid.is_some_and(|p| pid_is_ours(p, e.pid_start_time))
        })
        .filter_map(|e| {
            e.pid
                .map(|p| (e.name.clone(), p, e.harness_session_id.clone()))
        })
        .collect();
    // A collision on one candidate must not starve the rest: every candidate
    // in this tick gets attempted, and the first write failure is what's
    // returned (code-review finding on this commit) -- returning early on the
    // first `Err` left a persistently-colliding row at the front of the scan
    // starving every sibling candidate's bind, forever, since candidates are
    // rescanned in the same registry order on every subsequent sweep.
    let mut first_error: Option<String> = None;
    for (name, pid, predecessor) in candidates {
        let Some(sid) = probe(pid) else { continue };
        if predecessor.as_deref() == Some(sid.as_str()) {
            continue;
        }
        let predecessor_reachable = predecessor.as_deref().and_then(transition_probe);
        let classification = predecessor.as_deref().map(|previous| {
            state::classify_session_transition(previous, &sid, predecessor_reachable)
        });
        if predecessor.is_some() && predecessor_reachable.is_none() {
            continue;
        }
        let branch_name = format!("{name}-branch-{}", canonical_handle(&sid));
        let mut applied_transition: Option<state::SessionTransition> = None;
        let bound = match state::update_registry(&home.registry_json(), |r| {
            // A concurrent writer may have bound this row (or reaped it) since
            // the candidate scan above; never clobber a session id that
            // arrived in between.
            let current = r
                .find(&name)
                .and_then(|entry| entry.harness_session_id.clone());
            match (
                current.as_deref(),
                predecessor.as_deref(),
                classification,
                predecessor_reachable,
            ) {
                (None, None, None, None) => {
                    let Some(e) = r.find_mut(&name) else {
                        return false;
                    };
                    e.harness_session_id = Some(sid.clone());
                    true
                }
                (Some(previous), Some(sampled_predecessor), Some(_), Some(reachable))
                    if previous == sampled_predecessor && previous != sid =>
                {
                    match apply_session_transition(
                        r,
                        &name,
                        &sid,
                        Some(reachable),
                        &branch_name,
                        &sid,
                    ) {
                        Ok(applied) => {
                            applied_transition = Some(applied);
                            true
                        }
                        Err(_) => false,
                    }
                }
                _ => false,
            }
        }) {
            Ok(bound) => bound,
            Err(error) => {
                let message = format!("late-bind registry write failed for {name}: {error}");
                let _ = emitter.emit_fields(
                    "agent_late_bind_failed",
                    json_obj(&[
                        ("name", Value::String(name)),
                        ("pid", Value::Number(pid.into())),
                        ("harness_session_id", Value::String(sid)),
                        ("error", Value::String(error.to_string())),
                    ]),
                );
                first_error.get_or_insert(message);
                continue;
            }
        };
        if bound {
            let transition = applied_transition.map(|value| {
                Value::String(
                    match value {
                        state::SessionTransition::Succession => "succession",
                        state::SessionTransition::Branch => "branch",
                        state::SessionTransition::Deferred => "deferred",
                    }
                    .to_string(),
                )
            });
            let mut fields = vec![
                ("name", Value::String(name)),
                ("pid", Value::Number(pid.into())),
                ("harness_session_id", Value::String(sid)),
            ];
            if let (Some(predecessor), Some(transition)) = (predecessor, transition) {
                fields.push(("predecessor_session_id", Value::String(predecessor)));
                fields.push(("transition", transition));
            }
            let _ = emitter.emit_fields("agent_late_bind", json_obj(&fields));
        }
    }
    match first_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Apply one classified full-session transition under the registry writer.
/// Liveness is supplied by the existing family-1 truth probe; this function
/// does not infer it from status, pid, pane metadata, or argv.
#[allow(dead_code)]
pub(crate) fn apply_session_transition(
    registry: &mut state::Registry,
    predecessor_name: &str,
    successor_session_id: &str,
    predecessor_reachable: Option<bool>,
    branch_name: &str,
    branch_fno_id: &str,
) -> Result<state::SessionTransition, String> {
    let index = registry
        .entries
        .iter()
        .position(|entry| entry.name == predecessor_name)
        .ok_or_else(|| format!("unknown predecessor row {predecessor_name:?}"))?;
    let predecessor_session_id = registry.entries[index]
        .harness_session_id
        .as_deref()
        .unwrap_or("")
        .to_string();
    let classification = state::classify_session_transition(
        &predecessor_session_id,
        successor_session_id,
        predecessor_reachable,
    );
    match classification {
        state::SessionTransition::Succession => {
            if !registry.entries[index]
                .apply_succession(&predecessor_session_id, successor_session_id)
            {
                return Err("succession predecessor changed before apply".to_string());
            }
        }
        state::SessionTransition::Branch => {
            if branch_name.is_empty() || branch_fno_id.is_empty() {
                return Err("branch needs a distinct name and fno_id".to_string());
            }
            if let Some(existing) = registry
                .entries
                .iter()
                .find(|entry| entry.harness_session_id.as_deref() == Some(successor_session_id))
            {
                if existing.forked_from_session_id.as_deref() == Some(&predecessor_session_id)
                    && existing.fno_id.as_deref() == Some(branch_fno_id)
                {
                    return Ok(state::SessionTransition::Branch);
                }
                return Err(format!(
                    "branch successor session {successor_session_id:?} already has a row"
                ));
            }
            let branch_base = branch_name.to_string();
            let mut unique_branch_name = branch_base.clone();
            let mut suffix = 2;
            while registry
                .entries
                .iter()
                .any(|entry| entry.name == unique_branch_name)
            {
                unique_branch_name = format!("{branch_base}-{suffix}");
                suffix += 1;
            }
            if registry
                .entries
                .iter()
                .any(|entry| entry.fno_id.as_deref() == Some(branch_fno_id))
            {
                return Err(format!(
                    "branch fno_id {branch_fno_id:?} already has a registry row"
                ));
            }
            if registry
                .entries
                .iter()
                .any(|entry| entry.harness_session_id.as_deref() == Some(successor_session_id))
            {
                return Err(format!(
                    "branch successor session {successor_session_id:?} already has a row"
                ));
            }
            if registry.entries[index].fno_id.as_deref() == Some(branch_fno_id) {
                return Err("branch fno_id must be distinct from predecessor".to_string());
            }
            let branch = registry.entries[index].fork_for_session(
                &unique_branch_name,
                successor_session_id,
                &predecessor_session_id,
                branch_fno_id,
            );
            registry.entries.push(branch);
        }
        state::SessionTransition::Deferred => {}
    }
    Ok(classification)
}

/// Shell `fno agents codex-session-for-pid <pid>` -- the pane-tree rollout
/// walk (`_codex_session_id_for_pid`), reused rather than reimplemented in
/// Rust (Codex's rollout discovery needs a process-tree + open-fd walk this
/// crate has no dependency for; see `worktree_clean_probe` for the same
/// shell-and-parse-a-marker pattern). Fails closed to `None` on anything but
/// a clean exit with a non-empty `session_id=` line.
fn codex_session_for_pid_shellout(pid: u32) -> Option<String> {
    let out = std::process::Command::new("fno")
        .args(["agents", "codex-session-for-pid", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("session_id="))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Registry-side keeper sweep (x-ac6b).
// ---------------------------------------------------------------------------

/// How long one keeper probe waits for the Identify reply. A wedged keeper
/// (accepts the connection, never answers) must NAME its row inside this
/// bound and let startup continue - never wedge the daemon (AC4-ERR).
const KEEPER_SWEEP_REPLY_TIMEOUT: Duration = Duration::from_millis(750);
/// Budget for the whole sweep. These are local unix sockets, but a fleet of
/// wedged keepers each burning the reply timeout is still bounded work, and
/// the sweep shares the startup path with the accept loop.
const KEEPER_SWEEP_BUDGET: Duration = Duration::from_secs(10);

/// What one keeper socket probe concluded. The trisection mirrors the mux
/// pane sweep's `KeeperAdopt` (crates/fno/src/pty.rs): no listener is a
/// leftover to unlink, a live keeper is its socket's only address and is
/// NEVER unlinked, and silence is named rather than interpreted.
#[derive(Debug, PartialEq)]
enum KeeperProbe {
    /// The socket file exists but nothing accepts behind it: a dead keeper's
    /// leftover (the keeper unlinks on exit, so this is a kill -9 remainder).
    NoListener,
    /// The socket accepted the connection and stayed silent past the bound.
    /// Silence never proves death; the row is named and left untouched.
    Silent,
    /// A keeper answered Identify with this reply JSON.
    Answered(serde_json::Value),
}

/// Probe one keeper socket with the keeper binary's own frame codec: send
/// `Identify` via [`crate::pane_keeper::encode`], read the reply via
/// [`crate::pane_keeper::decode`]. Ring `Output` frames that share the burst
/// are skipped (this probe never takes the pty; it is not the subscriber).
fn probe_keeper_socket(sock: &Path, reply_timeout: Duration) -> KeeperProbe {
    use crate::pane_keeper::{decode, encode, Decode, Frame};
    use std::io::{Read, Write};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(sock) else {
        return KeeperProbe::NoListener;
    };
    let _ = stream.set_read_timeout(Some(reply_timeout));
    if stream.write_all(&encode(&Frame::Identify)).is_err() {
        return KeeperProbe::Silent;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    loop {
        // Drain whole frames already buffered before blocking on the socket.
        loop {
            match decode(&buf) {
                Decode::NeedMore => break,
                Decode::Violation(_) => return KeeperProbe::Silent,
                Decode::Frame(Frame::IdentifyReply(payload), _) => {
                    match serde_json::from_slice(&payload) {
                        Ok(value) => return KeeperProbe::Answered(value),
                        Err(_) => return KeeperProbe::Silent,
                    }
                }
                Decode::Frame(_, used) => {
                    buf.drain(..used);
                }
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return KeeperProbe::Silent,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// What the registry-side keeper sweep did, for the `keeper_sweep_done` event
/// and tests. Every dead or wedged verdict carries its reason; a verdict
/// without a named reason is exactly what AC3/AC4 exist to prevent.
#[derive(Debug, Default, PartialEq)]
pub struct KeeperSweepReport {
    /// Sockets examined.
    pub sockets: usize,
    /// Rows re-bound live (child pid asserted unchanged).
    pub rebound: Vec<String>,
    /// `(row, reason)` marked Exited.
    pub dead: Vec<(String, String)>,
    /// `(row, reason)` named but left untouched (silence never proves death).
    pub wedged: Vec<(String, String)>,
    /// Socket files unlinked (no listener behind them).
    pub unlinked: Vec<String>,
    /// Rows whose verdict was DISCARDED at write time: the registry row under
    /// that name changed identity between probe and write (removed and
    /// re-spawned under the same name while the daemon already served), so the
    /// probed verdict belongs to a row that no longer exists.
    pub superseded: Vec<String>,
}

/// The keeper socket directory for pane-less lane-B threads:
/// `<state-root>/mux/threads/`, beside the pane keepers' `mux/panes/`
/// (Python's `_lane_b_keeper_socket` writes there). Derived from the agents
/// root's parent the same way `quarantine_interrupted_write_temps` derives
/// the state root. This sweep and the mux pane sweep each own exactly one
/// directory - a thread socket has no tab and a pane socket has no row, so
/// neither discovery walks the other's ground.
fn lane_b_keeper_dir(home: &AgentsHome) -> PathBuf {
    home.root()
        .parent()
        .unwrap_or(home.root())
        .join("mux")
        .join("threads")
}

/// Stale store-socket hygiene: the store keeper unlinks its socket
/// on every clean exit, so a socket file nobody answers is a kill -9
/// leftover. The graph client self-heals a dead socket (its
/// connect-before-bind removes the stale file and rebinds), so this walk is
/// tidiness plus an honest dead count, never liveness authority: a socket
/// with a live listener is left exactly as found, and an unreadable one is
/// left for the process-table reaper (keeper_lane) rather than guessed at.
///
/// A state-root SIBLING socket is unlinked only when its graph file is gone
/// too: a rebind requires a client, a client requires the graph, so with the
/// graph absent no keeper can ever be behind the path and the probe-then-
/// unlink race with a self-healing client cannot happen. A sibling whose
/// graph still lives stays for the client's own connect-before-bind. The
/// hashed temp root is different: its contents are ours by construction and
/// its graph names are hashed away, so the probe alone decides.
pub fn store_socket_sweep(home: &AgentsHome, emitter: &EventEmitter) -> usize {
    // SAFETY: getuid reads a per-process kernel value; it cannot fail or race.
    let uid = unsafe { libc::getuid() };
    store_socket_sweep_in(
        home,
        std::env::temp_dir().join(format!("fno-store-{uid}")),
        emitter,
    )
}

/// The parameterized core, so tests point the hashed root at their own tree
/// instead of sweeping the machine's real one.
pub fn store_socket_sweep_in(
    home: &AgentsHome,
    temp_root: std::path::PathBuf,
    emitter: &EventEmitter,
) -> usize {
    let state_root = home.root().parent().unwrap_or(home.root()).to_path_buf();
    let dirs = vec![state_root, temp_root.clone()];
    let mut unlinked = 0;
    for dir in dirs {
        let in_temp_root = dir == temp_root;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_store_sock = if in_temp_root {
                // The hashed root is ours by construction: every .sock in it
                // is a store socket.
                name.starts_with(".fno-store-") && name.ends_with(".sock")
            } else {
                name.ends_with(".store.sock")
            };
            if !is_store_sock {
                continue;
            }
            let path = entry.path();
            // Sibling ownership rule: `<name>.store.sock` is only ours to
            // unlink when `<name>` (its graph) is gone. With the graph
            // present, a client rebind is always one connection away and
            // unlinking here could steal a socket a keeper just bound.
            if !in_temp_root {
                let graph = path.with_file_name(
                    path.file_name()
                        .map(|n| {
                            n.to_string_lossy()
                                .trim_end_matches(".store.sock")
                                .to_string()
                        })
                        .unwrap_or_default(),
                );
                if graph.exists() {
                    continue;
                }
            }
            let dead = match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => {
                    // A live keeper is behind it: leave the socket alone.
                    drop(stream);
                    false
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        || e.kind() == std::io::ErrorKind::NotFound
                        // macOS answers ENOTSOCK when the path is not a
                        // socket at all (Linux says ECONNREFUSED); either
                        // way nothing can ever be listening behind it, so
                        // the litter is safe to unlink.
                        || e.raw_os_error() == Some(libc::ENOTSOCK) =>
                {
                    true
                }
                Err(_) => false, // unreadable is not dead; the reaper owns that verdict
            };
            if dead && std::fs::remove_file(&path).is_ok() {
                unlinked += 1;
                let _ = emitter.emit(
                    "store_socket_unlinked",
                    &json!({"path": path.to_string_lossy()}),
                );
            }
        }
    }
    unlinked
}

/// One planned row mutation out of the sweep. `bound_socket`/`bound_session`
/// carry the probed row's immutable identity (the socket it was found by, and
/// its session id at probe time) so the write can revalidate under the
/// registry lock: probing runs up to the sweep budget while the daemon is
/// already serving, and an operator can remove and re-spawn a row under the
/// SAME name in that window. A name-only apply would stamp the old keeper's
/// verdict (or child pid) onto the healthy replacement.
struct KeeperSweepChange {
    name: String,
    status: Option<AgentStatus>,
    child_pid: Option<u32>,
    /// `Some` when the row was bound by socket; `None` when the session-id
    /// fallback found a row carrying no socket.
    bound_socket: Option<String>,
    bound_session: Option<String>,
}

/// Apply the sweep's planned changes under the registry lock, revalidating
/// each row's identity first. Returns the names whose verdicts were discarded
/// because the row changed identity between probe and write.
fn apply_keeper_sweep_changes(
    registry: &mut state::Registry,
    changes: &[KeeperSweepChange],
    now: &str,
) -> Vec<String> {
    let mut superseded = Vec::new();
    for change in changes {
        let Some(entry) = registry.find_mut(&change.name) else {
            // The row was removed outright: the verdict dies with it.
            superseded.push(change.name.clone());
            continue;
        };
        let identity_holds = entry.messaging_socket_path == change.bound_socket
            && entry.harness_session_id == change.bound_session;
        if !identity_holds {
            superseded.push(change.name.clone());
            continue;
        }
        apply_reconcile_change(entry, change.status, None, now);
        if let Some(pid) = change.child_pid {
            entry.keeper_child_pid = Some(pid);
        }
    }
    superseded
}

/// Re-bind surviving lane-B keeper threads to their registry rows at daemon
/// start (x-ac6b): the registry-side consumer of the keeper discovery, keyed
/// on the row rather than on a mux member (a lane-B thread has no tab, so
/// the mux server's re-adopt sweep never sees its socket).
///
/// A keeper-hosted thread survives a daemon death by construction - the
/// keeper holds the pty master and ignores SIGHUP - but the registry's
/// knowledge of it does not. This sweep walks each thread socket, Identifies
/// the keeper behind it (same frames, same binary; the mux pane sweep in
/// `crates/fno/src/server.rs::keeper_readopt` is the first consumer of that
/// discovery), and reconciles against the row by harness session id with the
/// child pid as the assertion: a socket answering a DIFFERENT session id, or
/// the same id from a different child, is a respawn wearing the row's name,
/// and is named dead - never silently re-bound to a fresh session (AC3-ERR).
///
/// Ordering (the daemon-side twin of the pane sweep's re-adopt-before-restore
/// hazard): the caller runs this BEFORE the startup reconcile sweep, so the
/// settle pass reads rows the sweep already re-bound. Strictly non-fatal: an
/// unreadable registry is an Err the caller emits and serves past, matching
/// the reconcile sweep's degradation posture.
pub fn keeper_registry_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
) -> Result<KeeperSweepReport, String> {
    let dir = lane_b_keeper_dir(home);
    let mut sockets: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".sock") {
                sockets.push(entry.path());
            }
        }
    }
    sockets.sort();
    let mut report = KeeperSweepReport {
        sockets: sockets.len(),
        ..KeeperSweepReport::default()
    };
    if sockets.is_empty() {
        return Ok(report);
    }
    let registry = load_registry_asserted(&home.registry_json())
        .map_err(|e| format!("registry read failed: {e}"))?;

    let start = Instant::now();
    let mut changes: Vec<KeeperSweepChange> = Vec::new();
    for (idx, sock) in sockets.iter().enumerate() {
        if start.elapsed() >= KEEPER_SWEEP_BUDGET {
            let _ = emitter.emit(
                "keeper_sweep_budget_exhausted",
                &json!({"remaining": sockets.len() - idx}),
            );
            break;
        }
        let sock_str = sock.to_string_lossy().into_owned();
        // The row is bound by its own socket first; a row whose socket field
        // was lost but whose identity matches is still found by session id.
        let row = registry
            .entries
            .iter()
            .find(|e| e.messaging_socket_path.as_deref() == Some(sock_str.as_str()));
        let probe = probe_keeper_socket(sock, KEEPER_SWEEP_REPLY_TIMEOUT);
        match probe {
            KeeperProbe::NoListener => {
                // The keeper unlinks its socket on every exit path, so a
                // socket file with nobody behind it is a kill -9 leftover.
                // Unlink it (the stale-socket contract) and name the row.
                let _ = std::fs::remove_file(sock);
                report.unlinked.push(sock_str.clone());
                let _ = emitter.emit("keeper_socket_unlinked", &json!({"path": sock_str}));
                if let Some(row) = row {
                    let reason = "keeper socket has no listener behind it".to_string();
                    let _ = emitter.emit(
                        "keeper_row_dead",
                        &json!({"name": row.name, "reason": reason}),
                    );
                    changes.push(KeeperSweepChange {
                        name: row.name.clone(),
                        status: Some(AgentStatus::Exited),
                        child_pid: None,
                        bound_socket: Some(sock_str.clone()),
                        bound_session: row.harness_session_id.clone(),
                    });
                    report.dead.push((row.name.clone(), "no listener".into()));
                }
            }
            KeeperProbe::Silent => {
                // AC4-ERR: named, never interpreted. The socket STAYS - a
                // live listener is the thread's only address.
                if let Some(row) = row {
                    let reason =
                        format!("keeper accepted but did not answer Identify within {KEEPER_SWEEP_REPLY_TIMEOUT:?}");
                    let _ = emitter.emit(
                        "keeper_row_wedged",
                        &json!({"name": row.name, "reason": reason}),
                    );
                    report.wedged.push((row.name.clone(), reason));
                } else {
                    let _ = emitter.emit("keeper_socket_silent_no_row", &json!({"path": sock_str}));
                }
            }
            KeeperProbe::Answered(reply) => {
                let str_field = |key: &str| -> Option<String> {
                    reply
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                };
                let answered_session = str_field("session_id");
                let answered_child = reply
                    .get("child_pid")
                    .and_then(serde_json::Value::as_u64)
                    .map(|p| p as u32);
                let answered_cwd = str_field("cwd");
                let bound_socket = row.is_some().then(|| sock_str.clone());
                let row = row.or_else(|| {
                    // Session-id fallback: the reconciliation key the plan
                    // names. Only for a row with no socket of its own.
                    registry.entries.iter().find(|e| {
                        e.messaging_socket_path.as_deref().is_none()
                            && answered_session.is_some()
                            && e.harness_session_id == answered_session
                    })
                });
                let Some(row) = row else {
                    let _ = emitter.emit(
                        "keeper_socket_orphan",
                        &json!({
                            "path": sock_str,
                            "session_id": answered_session,
                        }),
                    );
                    continue;
                };
                // Identity triple, each leg named on mismatch: session id
                // (the reconciliation key), child pid (the respawn catcher),
                // cwd (byte-equal, the same directory across the restart).
                // Each leg evaluates INDEPENDENTLY - an else-if chain would
                // skip the cwd check whenever both child pids are present and
                // equal, re-binding a keeper that moved directories.
                let session_mismatch = (row.harness_session_id != answered_session).then(|| {
                    format!(
                        "keeper answers session id {answered_session:?}, row stores {:?}",
                        row.harness_session_id
                    )
                });
                let pid_mismatch = match (row.keeper_child_pid, answered_child) {
                    (Some(recorded), Some(answered)) if recorded != answered => Some(format!(
                        "child pid changed: row records {recorded}, keeper answers {answered}"
                    )),
                    _ => None,
                };
                let cwd_mismatch = answered_cwd.as_deref().and_then(|answered_cwd| {
                    (!answered_cwd.is_empty() && answered_cwd != row.cwd).then(|| {
                        format!(
                            "keeper cwd {answered_cwd:?} differs from row cwd {:?}",
                            row.cwd
                        )
                    })
                });
                let mismatch = session_mismatch.or(pid_mismatch).or(cwd_mismatch);
                if let Some(reason) = mismatch {
                    let _ = emitter.emit(
                        "keeper_row_dead",
                        &json!({"name": row.name, "reason": reason}),
                    );
                    changes.push(KeeperSweepChange {
                        name: row.name.clone(),
                        status: Some(AgentStatus::Exited),
                        child_pid: None,
                        bound_socket: bound_socket.clone(),
                        bound_session: row.harness_session_id.clone(),
                    });
                    report.dead.push((row.name.clone(), reason));
                    continue;
                }
                // Identity holds. A terminal row is never resurrected by this
                // sweep (that recovery is reconcile's Orphaned->Live arm);
                // re-bind only a row that is still live-ish or orphaned.
                let rebindable =
                    !matches!(row.status, AgentStatus::Exited | AgentStatus::PermanentDead);
                if rebindable {
                    let _ = emitter.emit(
                        "keeper_row_rebound",
                        &json!({
                            "name": row.name,
                            "child_pid": answered_child,
                            "session_id": answered_session,
                        }),
                    );
                    changes.push(KeeperSweepChange {
                        name: row.name.clone(),
                        status: Some(AgentStatus::Live),
                        child_pid: answered_child,
                        bound_socket: bound_socket.clone(),
                        bound_session: row.harness_session_id.clone(),
                    });
                    report.rebound.push(row.name.clone());
                } else {
                    let _ = emitter.emit(
                        "keeper_row_terminal_socket_live",
                        &json!({"name": row.name, "status": row.status}),
                    );
                }
            }
        }
    }
    if changes.is_empty() {
        return Ok(report);
    }
    let now = now_rfc3339_like();
    let mut superseded = Vec::new();
    state::update_registry(&home.registry_json(), |r| {
        superseded = apply_keeper_sweep_changes(r, &changes, &now);
    })
    .map_err(|e| format!("keeper sweep registry write failed: {e}"))?;
    if !superseded.is_empty() {
        let _ = emitter.emit("keeper_row_superseded", &json!({"names": superseded}));
        report.superseded = superseded;
    }
    Ok(report)
}

fn run_reconcile_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    thread_hosted: &dyn Fn(&RegistryEntry) -> bool,
) -> Result<ReconcileSweepResult, String> {
    use crate::provider::ReachabilityProbeError;

    // Late bind (x-9de7 task 2), before the registry snapshot below is taken,
    // so a row bound this tick is already visible to the probe/reconcile pass
    // that follows.
    late_bind_codex_sessions(home, emitter, &codex_session_for_pid_shellout)?;

    // x-4c87: a broken registry is a failed sweep, never a successful zero-row
    // scan. `unwrap_or_default()` here answered the client-facing reconcile
    // RPC with `scanned: 0` over a store full of rows (code-review on PR 924).
    let registry = match load_registry_asserted(&home.registry_json()) {
        Ok(r) => r,
        Err(e) => return Err(format!("registry read failed: {e}")),
    };

    // Fairness: probe least-recently-reconciled first (None < Some), so a
    // budget-exhausted sweep eventually covers every entry (finding #1).
    let mut entries = registry.entries.clone();
    entries.sort_by(|a, b| a.last_reconciled_at.cmp(&b.last_reconciled_at));

    let probe = |e: &RegistryEntry| -> Result<bool, ReachabilityProbeError> {
        // Fast path: a reachable worker socket is authoritative, PID-reuse-immune
        // liveness for a PTY-managed agent — no provider probe (and no 250ms
        // cost) needed. A sync connect is fine: reconcile runs on the blocking
        // pool (Codex P1: do not trust a possibly-stale registry pid).
        if std::os::unix::net::UnixStream::connect(home.worker_sock(&e.short_id)).is_ok() {
            return Ok(true);
        }
        // No live worker: ask the provider's session store (tri-state).
        match crate::provider::for_name(e.harness_name()) {
            Some(p) => p.reachability(&to_agent_entry(e), RECONCILE_PROBE_TIMEOUT),
            None => Err(ReachabilityProbeError::new(
                e.harness_name(),
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
    // Liveness for a `claude --substrate bg` thread, which carries neither a
    // footnote pid nor a worker socket: claude's own daemon roster is the only
    // truth. Read once per sweep, not per row. A MISSING roster parses as zero
    // workers (no claude daemon ever ran) and reaps as before; an UNREADABLE one
    // is unknown liveness, where we refuse to declare death -- a false `exited`
    // on a working teammate costs a duplicate spawn, a stale `live` costs a
    // waiter its timeout.
    let roster = crate::claude_roster::ClaudeRoster::load_default();
    // The x-5d96 zombie flip fires only when the roster read SUCCEEDED: an
    // unreadable roster is unknown liveness (the fail-closed branch below),
    // and orphaning a live worker on a transient instrumentation failure is
    // the exact false positive the flip must not produce (codex P1, PR 1329).
    let roster_readable = roster.is_ok();
    let bg_live = |e: &RegistryEntry| -> bool {
        if e.harness_name() != "claude" {
            return false;
        }
        match &roster {
            Ok(r) => {
                r.find(&e.short_id).is_some()
                    || e.harness_session_id
                        .as_deref()
                        .is_some_and(|sid| r.find(sid).is_some())
            }
            Err(_) => true,
        }
    };
    // The rollout file recorded at spawn is the durable codex thread object
    // (docs/architecture/codex-thread-driver.md); its existence is what makes
    // an unhosted thread Orphaned (resumable) instead of Exited.
    let rollout_exists = |e: &RegistryEntry| -> bool {
        e.log_path
            .as_deref()
            .map(Path::new)
            .is_some_and(Path::is_file)
    };
    // The session-names overlay folds into the rows on every sweep:
    // best-effort, one small file read, and the count is an event.
    crate::session_names_fold::fold_session_names(home, emitter);
    let probes = batched_row_probes(&entries, &crate::truth_probe::family1_truth_probe_many);
    // One batch feeds both consumers: the ladder's truth rung reads states,
    // the title detector reads titles. The probes are keyed by the row's
    // claude uuid (the handle the batch asked for), which is also the map
    // key the row lookup below uses.
    let truth: std::collections::HashMap<String, String> = probes
        .iter()
        .map(|(h, p)| (h.clone(), p.state.clone()))
        .collect();
    let titles: std::collections::HashMap<String, Option<String>> = probes
        .into_iter()
        .map(|(h, p)| (h, p.harness_title))
        .collect();
    // Title diff, computed off the SAME snapshot the write below
    // applies to: the harness's own name for the session against the row's
    // last-seen value. `name` is NEVER written from it - the label is fno's,
    // the title is the harness's - and the emit rides the successful write,
    // so a failed write never announces a rename it did not persist.
    let renames = title_changes(&entries, &titles);
    let prober = live_liveness_prober(truth);
    // The sweep budget starts HERE, after the truth batch and the
    // roster load: those reads serve every verb, and charging them to the
    // probe loop's 5s window was why 79 rows went unprobed every sweep
    // (24s wall, 0 probed). The probe loop and the roster-progress loop
    // below share this one clock.
    let start = Instant::now();
    let (changes, outcome) = plan_reconcile(
        &entries,
        probe,
        || start.elapsed() >= RECONCILE_SWEEP_BUDGET,
        pid_live,
        bg_live,
        thread_hosted,
        rollout_exists,
        prober,
        roster_readable,
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
            // Keyed on the probed row's identity read off
            // the same snapshot the sweep planned from, so a row replaced
            // under the same label between snapshot and locked write cannot
            // receive the first row's status.
            let ident = entries
                .iter()
                .find(|e| e.name == ch.name)
                .map(state::registry_write_key);
            let keyed = ident
                .as_ref()
                .and_then(|(h, sid)| sid.as_deref().and_then(|sid| r.find_by_session_mut(h, sid)));
            let target = match keyed {
                Some(e) => Some(e),
                None => r.find_mut(&ch.name),
            };
            if let Some(e) = target {
                apply_reconcile_change(e, ch.new_status, ch.new_liveness, &now);
            }
        }
        // Apply the batch's title readings in the SAME lock window:
        // the row's stored title is the diff baseline the next sweep compares
        // against, so a row the reconcile changes never skipped lost its
        // rename.
        apply_title_changes(r, &entries, &titles);
    }) {
        let _ = emitter.emit("reconcile_error", &json!({"error": err.to_string()}));
        return Err(format!(
            "reconcile computed {} change(s) but the registry write failed: {err}",
            changes.len()
        ));
    }

    // The renames ride the SUCCESSFUL write: each event names the
    // row whose stored title the write just advanced, so events.jsonl never
    // announces a rename the registry does not carry, and a failed write
    // (the early return above) never announces one either.
    for (name, sid, from, to) in &renames {
        let _ = emitter.emit(
            "agent_renamed",
            &json!({
                "name": name,
                "harness_session_id": sid,
                "from": from,
                "to": to,
            }),
        );
    }

    // Roster-progress refresh (x-cdc7 SECOND HALF): the same per-tick set the
    // reconcile sweep just probed - but this loop's own git/gh subprocess
    // calls are NOT covered by the probe loop's budget check above (that one
    // stops feeding `plan_reconcile` new entries; it does not bound what runs
    // after). Re-check the SAME `start`/`RECONCILE_SWEEP_BUDGET` clock here so
    // a large changed-row set cannot extend a sweep that runs synchronously at
    // daemon startup and blocks `accept()` on every `reconcile` RPC. Remaining
    // rows are simply deferred to the next tick, the same fairness the probe
    // loop itself relies on. Best-effort and non-fatal otherwise: an I/O
    // failure here must never fail the sweep that already wrote the registry.
    let progress_path = home.roster_progress_json();
    for ch in &changes {
        if start.elapsed() >= RECONCILE_SWEEP_BUDGET {
            break;
        }
        let Some(e) = entries.iter().find(|e| e.name == ch.name) else {
            continue;
        };
        if e.cwd.is_empty() {
            continue;
        }
        if let Err(err) =
            crate::roster_progress::refresh_row(&progress_path, &e.name, Path::new(&e.cwd), &now)
        {
            eprintln!(
                "reconcile: roster-progress refresh failed for {}: {err}",
                e.name
            );
        }
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

/// The agent.rename route. state.rs owns the grammar and the transaction.
/// `agent.watch`: the subscription face of the registry.
///
/// `{"since": {"mtime_nanos", "len"} | null}` in; one answer out. The first
/// call (`since` absent) serves the FULL document - connect, payload. Later
/// calls serve the full document again only when the registry's (mtime, len)
/// stamp moved - which is exactly what any write (the sweep, `agent.report`,
/// spawn, rm, a Python-side CLI verb) does to the file - and a bare version
/// echo when it did not, so a polling reader costs one stat per tick instead
/// of one file read. The caller keeps its read off the file entirely: the
/// daemon is the reader now, the served rows are the served facts.
fn handle_watch(ctx: &Ctx, req: &Request) -> Response {
    let since = req.params.get("since").and_then(|v| {
        let mtime = v.get("mtime_nanos")?.as_i64()?;
        let len = v.get("len")?.as_u64()?;
        Some((mtime, len))
    });
    let path = ctx.home.registry_json();
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        // A vanished registry is a legitimate empty answer, not an error: the
        // watcher clears (the same contract the file reader's vanish arm has).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::ok(
                req.id,
                json!({"version": Value::Null, "doc": {"agents": []}}),
            );
        }
        Err(e) => {
            return registry_read_failed(req.id, state::StateError::Io(e));
        }
    };
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let len = meta.len();
    let version = json!({"mtime_nanos": mtime_nanos, "len": len});
    let unchanged = matches!(&since, Some((m, l)) if *m == mtime_nanos && *l == len);
    if unchanged {
        return Response::ok(req.id, json!({"version": version, "doc": null}));
    }
    let registry = match load_registry_asserted(&path) {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
    match serde_json::to_value(&registry) {
        Ok(doc) => Response::ok(req.id, json!({"version": version, "doc": doc})),
        Err(e) => Response::err(
            req.id,
            ErrorCode::Internal,
            format!("watch: registry serialize failed: {e}"),
        ),
    }
}

fn handle_rename(ctx: &Ctx, req: &Request) -> Response {
    state::rename_response(&ctx.home.registry_json(), req)
}

fn handle_reconcile(ctx: &Ctx, req: &Request) -> Response {
    let ReconcileSweepResult {
        registry,
        entries,
        outcome,
    } = match run_reconcile_sweep(&ctx.home, &ctx.emitter, &|entry: &RegistryEntry| {
        match ctx.codex_threads.try_lock() {
            Ok(guard) => guard.contains_key(&entry.name),
            // An actor is mid insert/remove: hosted, so a race can never
            // settle a thread the map is about to name.
            Err(_) => true,
        }
    }) {
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
        .map(|e| json!({"name": e.name, "provider": e.harness_name()}))
        .collect();
    let orphaned_py: Vec<Value> = outcome
        .orphans
        .iter()
        .map(|n| {
            let prov = registry
                .entries
                .iter()
                .find(|e| &e.name == n)
                .map(|e| e.harness_name())
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
                .map(|e| e.harness_name())
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
/// Detached inside `operator_notice::notify_operator` so a missing or slow
/// `fno inbox notify` can never stall the registry write that observed the
/// transition - the same bounded/fail-open discipline as the external
/// claim-status writer that once froze admit. A spawn failure is logged and
/// dropped; the registry write that called this has already succeeded.
pub(crate) fn notify_transition(title: String, body: String) {
    crate::operator_notice::notify_operator(&title, &body, None);
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
        if e.harness_name() != "claude" || e.claude_session_uuid.is_some() {
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
    // and map to the typed enum for storage. `model` is the
    // PostModelSwitch posture: no inside-leg transition, the report only
    // diffs the row's SERVED model/effort axes, and it must carry at least
    // one of them.
    let state_label = match req.params.get("state").and_then(|v| v.as_str()) {
        Some(s @ ("working" | "blocked" | "done" | "model")) => s.to_string(),
        _ => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "`state` must be working|blocked|done|model",
            )
        }
    };
    let model_only = state_label == "model";
    let model = req
        .params
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let effort = req
        .params
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    if model_only && model.is_none() && effort.is_none() {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "state=model requires `model` or `effort`",
        );
    }
    let state = match state_label.as_str() {
        "working" => Some(state::InsideLegState::Working),
        "blocked" => Some(state::InsideLegState::Blocked),
        "done" => Some(state::InsideLegState::Done),
        _ => None,
    };
    let reason = req
        .params
        .get("reason")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ttl_ms = req.params.get("ttl_ms").and_then(|v| v.as_u64());

    // Build the report once; a clone moves into the locked store path, the
    // original is reused for the early-push buffer when no row exists yet.
    // `None` under the model posture: there is no transition to store.
    let report = state.map(|state| state::InsideLegReport {
        state,
        seq,
        reason,
        received_at: now_rfc3339_like(),
        ttl_ms,
    });
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
    // The row's label, captured under the flock for the axis-change
    // events emitted after the write.
    let mut entry_name: Option<String> = None;
    // Served-axis change records captured under the flock, emitted
    // after the write: (kind, from, to). `requested_*` are never touched -
    // they stay the spawn request, which is the provenance.
    let mut axis_changes: Vec<(&str, Option<String>, String)> = Vec::new();
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
        entry_name = Some(entry.name.clone());
        if let Some(rep) = &report_for_store {
            if let Some(prev) = &entry.inside_leg {
                if seq <= prev.seq {
                    outcome = Outcome::StaleSeq { last: prev.seq };
                    return;
                }
            }
            let prev_state = entry.inside_leg.as_ref().map(|r| r.state);
            if state::enters(prev_state, rep.state, state::InsideLegState::Blocked) {
                let body = rep.reason.clone().unwrap_or_else(|| state_label.clone());
                notify = Some((entry.name.clone(), body, false));
            } else if state::enters(prev_state, rep.state, state::InsideLegState::Done) {
                let body = rep.reason.clone().unwrap_or_else(|| state_label.clone());
                notify = Some((entry.name.clone(), body, true));
            }
            entry.inside_leg = Some(rep.clone());
            // Capability flip: the hook now owns this row's signal; a stale
            // scrape verdict must never shadow it (per-capability arbitration).
            entry.screen_state = None;
        }
        if let Some(m) = &model {
            if entry.model.as_deref() != Some(m.as_str()) {
                axis_changes.push(("agent_model_changed", entry.model.clone(), m.clone()));
                entry.model = Some(m.clone());
                entry.model_basis = Some("verified".to_string());
            }
        }
        if let Some(eff) = &effort {
            if entry.effort.as_deref() != Some(eff.as_str()) {
                axis_changes.push(("agent_effort_changed", entry.effort.clone(), eff.clone()));
                entry.effort = Some(eff.clone());
            }
        }
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
            // One event per served-axis change, emitted only after
            // the write landed.
            for (kind, from, to) in &axis_changes {
                let _ = ctx.emitter.emit(
                    kind,
                    &json!({
                        "name": entry_name,
                        "harness_session_id": session_id,
                        "from": from,
                        "to": to,
                    }),
                );
            }
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
        // a poisoned lock -> `None` -> the old hard-drop degrade. A
        // model-posture report has no transition to buffer: an unknown session
        // is a plain drop.
        Outcome::Unknown => {
            let buffered = report
                .map(|rep| {
                    ctx.pending_inside_leg
                        .lock()
                        .map(|mut buf| buffer_pending_report(&mut buf, &session_id, rep))
                        .ok()
                })
                .flatten();
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
    // Optional `envelope`: present-but-not-an-object is a client error, rejected
    // BEFORE any registry or sidecar work. Absent -> legacy confirm-only response.
    let envelope = match req.params.get("envelope") {
        None => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(_) => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "`envelope` must be a JSON object",
            )
        }
    };
    // x-4c87: a channel lookup over an unreadable registry reports the failed
    // read, never a false `ChannelUnknown` for a channel its rows carry. The
    // asserted read also refuses a partial roster (this handler is sync, so it
    // takes the blocking read inline as before).
    let registry = match load_registry_asserted(&ctx.home.registry_json()) {
        Ok(reg) => reg,
        Err(e) => return registry_read_failed(req.id, e),
    };
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
    let envelope = match envelope {
        Some(e) => e,
        None => {
            // Confirm-only: the route exists; delivery is the channel server's job.
            return Response::ok(req.id, json!({"routed": true}));
        }
    };
    // Deliver via the Python sidecar (`fno agents mcp send`), inheriting its lazy-start
    // + socket discovery instead of reimplementing it in Rust. `delivered: true`
    // only when the sidecar accepted the envelope; on failure `reason` is
    // MANDATORY so a caller can tell route-exists from delivered.
    match deliver_envelope(&channel_id, &envelope) {
        Ok(()) => Response::ok(req.id, json!({"routed": true, "delivered": true})),
        Err(reason) => Response::ok(
            req.id,
            json!({"routed": true, "delivered": false, "reason": reason}),
        ),
    }
}

/// Shell `fno agents mcp send --session <id>` with `envelope` on stdin (never argv - it
/// can be large). Returns `Err(reason)` on any failure (spawn or non-zero exit),
/// with the stderr tail as the reason.
fn deliver_envelope(channel_id: &str, envelope: &Value) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = crate::loop_dispatch::fno_cmd("fno")
        .args(["agents", "mcp", "send", "--session-id", channel_id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `fno agents mcp send` failed: {e}"))?;
    // Write + close stdin (drop => EOF) so the child's `stdin.read()` completes.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child stdin unavailable".to_string())?;
        let bytes = serde_json::to_vec(envelope).map_err(|e| format!("serialize envelope: {e}"))?;
        stdin
            .write_all(&bytes)
            .map_err(|e| format!("write envelope to `fno agents mcp send`: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait for `fno agents mcp send`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail = stderr.trim().rsplit('\n').next().unwrap_or("").trim();
    Err(if tail.is_empty() {
        format!("`fno agents mcp send` exited {}", out.status)
    } else {
        tail.to_string()
    })
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
    #[path = "store_socket_sweep_tests.rs"]
    mod store_socket_sweep_tests;
    use super::*;
    use std::io::Write;

    /// The e2e restart-storm test only exercises `state_error_code` when the
    /// scheduler happens to race a task into shutdown-cancellation, so its
    /// coverage of the Cancelled -> ShuttingDown mapping is real but silent
    /// on a run where nothing races. Pin the mapping directly and
    /// deterministically: Cancelled must classify as ShuttingDown, and every
    /// other StateError variant must stay Internal.
    #[test]
    fn state_error_code_classifies_cancelled_as_shutting_down() {
        assert_eq!(
            state_error_code(&state::StateError::Cancelled("task cancelled".into())),
            ErrorCode::ShuttingDown
        );
        assert_eq!(
            state_error_code(&state::StateError::Io(std::io::Error::other("boom"))),
            ErrorCode::Internal
        );
        assert_eq!(
            state_error_code(&state::StateError::InvariantViolation("drift".into())),
            ErrorCode::Internal
        );
    }

    /// Registry-local projection used only by the address-form unit test.
    fn canonical_name_in(registry: &state::Registry, token: &str) -> String {
        let Ok(Value::Array(rows)) = serde_json::to_value(&registry.entries) else {
            return token.to_string();
        };
        match crate::client_verbs::find_agent_entry(&rows, token) {
            Ok(entry) => entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(token)
                .to_string(),
            Err(_) => token.to_string(),
        }
    }
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

    // x-ef7f: the connect-probe singleton guard let a busy-but-alive
    // incumbent read as absent, so every losing race added a new supervisor
    // instead of replacing the incumbent. The flock-based guard must resolve
    // N concurrent binders to exactly one winner.
    #[tokio::test]
    async fn bind_supervisor_socket_concurrent_only_one_survives() {
        // A real UnixListener::bind needs its path to fit sockaddr_un's short
        // sun_path buffer (104 bytes on macOS); tmp_home()'s long
        // tag+pid+nanos name overflows that once `/supervisor.sock` is
        // appended, so this test (the first here to actually bind a socket)
        // builds a short path directly under /tmp instead.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::path::PathBuf::from(format!("/tmp/fa-cb-{}-{n}", std::process::id()));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let h = home.clone();
            handles.push(tokio::spawn(
                async move { bind_supervisor_socket(&h).await },
            ));
        }
        // Collect every result FIRST, then count, so no winner's lock guard is
        // dropped while another task is still trying for it. Counting inside
        // the join loop released the lock at the first `Ok(_)` and handed it to
        // a task still mid-retry, which read as three winners -- an artifact of
        // the test's own teardown order, not of the guard. In production the
        // holder keeps its guard for the whole process lifetime, which is what
        // this shape reproduces.
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }
        let mut ok_count = 0;
        let mut already_running = 0;
        for r in &results {
            match r {
                Ok(_) => ok_count += 1,
                Err(DaemonError::AlreadyRunning(_)) => already_running += 1,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert_eq!(ok_count, 1, "exactly one bind must survive the race");
        assert_eq!(already_running, 7);
        drop(results);
        std::fs::remove_dir_all(home.root()).ok();
    }

    // x-ef7f / x-e98b: the bound-inode check is what lets a daemon detect
    // that its socket path was unlinked and rebound out from under it (by an
    // operator `rm`, or a departing incumbent's blind unlink) instead of
    // continuing to serve unreachable, and lets the exit-time cleanup refuse
    // to unlink a live successor's fresh socket.
    #[test]
    fn socket_inode_matches_detects_unlink_and_rebind() {
        let home = tmp_home("inode-retire");
        let sock = home.supervisor_sock();
        // HOLD the original open for the whole test. In production the daemon
        // is listening on this inode, which is what keeps its number from being
        // recycled. Dropping the handle first frees the number, and Linux hands
        // the very same one to the next file at that path -- the rebind then
        // reads as a match and the test fails there while passing on macOS.
        let held = std::fs::File::create(&sock).unwrap();
        let ino = held.metadata().unwrap().ino();
        assert!(socket_inode_matches(&sock, ino));

        std::fs::remove_file(&sock).unwrap();
        std::fs::write(&sock, b"").unwrap(); // a new inode takes the same path
        assert!(
            !socket_inode_matches(&sock, ino),
            "a rebound path must not match the old inode"
        );
        drop(held);
    }

    fn read_events(home: &AgentsHome) -> Vec<Value> {
        std::fs::read_to_string(home.events_jsonl())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    #[test]
    fn merge_cleanup_fold_keeps_requests_until_completed_or_refused() {
        let home = tmp_home("merge-cleanup-fold");
        let request = json!({
            "ts": "2026-09-02T00:00:00Z",
            "type": "merge_cleanup_requested",
            "source": "python",
            "data": {
                "request_id": "merge-cleanup-1",
                "repo": "/repo",
                "pr": 42,
                "branch": "feature/session",
                "worktree": "/repo/worktree",
                "node_ids": ["x-90ee"]
            }
        });
        std::fs::write(
            home.events_jsonl(),
            format!("{}\n", serde_json::to_string(&request).unwrap()),
        )
        .unwrap();
        assert_eq!(pending_merge_cleanup_requests(&home, "/repo").len(), 1);
        assert!(merge_cleanup_requested(&home, "/repo"));

        let completed = json!({
            "ts": "2026-09-02T00:01:00Z",
            "type": "merge_cleanup_completed",
            "source": "daemon",
            "data": {
                "request_id": "merge-cleanup-1",
                "repo": "/repo",
                "pr": 42,
                "reclaimed_bytes": 12
            }
        });
        std::fs::OpenOptions::new()
            .append(true)
            .open(home.events_jsonl())
            .unwrap()
            .write_all(format!("{}\n", serde_json::to_string(&completed).unwrap()).as_bytes())
            .unwrap();
        assert!(pending_merge_cleanup_requests(&home, "/repo").is_empty());
        assert!(!merge_cleanup_requested(&home, "/repo"));
        std::fs::remove_dir_all(home.root()).ok();
    }

    // Generic one-shot ask row builder (empty short_id + no pid, owns no
    // worktree). The `exited_at` argument predates reverse-join retirement;
    // it now only feeds the liveness ladder's heartbeat rung.
    fn ask_row(name: &str, exited_at: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: name.into(),
            short_id: String::new(),
            legacy_provider: "claude".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            // x-7bcd: needs a resolvable handle (leg 3); deterministic per
            // name so two rows never collide.
            harness_session_id: Some(format!("{name}-sess")),
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            spawn_trigger: None,
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
            keeper_child_pid: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: exited_at.map(str::to_string),
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            sandbox_posture: None,
            ..Default::default()
        }
    }

    fn claude_rm_row(name: &str, short_id: &str, session_id: &str) -> RegistryEntry {
        let mut row = ask_row(name, Some("2020-01-01T00:00:00Z"));
        row.short_id = short_id.to_string();
        row.harness = Some("claude".into());
        row.harness_session_id = Some(session_id.into());
        row
    }

    #[test]
    fn row_identity_matches_treats_a_none_session_capture_as_unasserted() {
        // The one shared comparator both `handle_rm_with`'s retain and
        // `switchboard_identity_matches` now call through (self-review
        // finding #8): a session id captured as `None` (a codex row before
        // `late_bind_codex_sessions` binds it) must not read as a mismatch
        // against the SAME row's later `Some`.
        let row = claude_rm_row("worker", "short1", "session-abc");
        let unasserted = RowIdentity {
            harness: None,
            name: Some("worker"),
            short_id: "short1",
            session_id: None,
            created_at: "2020-01-01T00:00:00Z",
        };
        assert!(row_identity_matches(&row, &unasserted));

        // A captured `Some` that disagrees with the row IS a real mismatch.
        let disagreeing = RowIdentity {
            session_id: Some("some-other-session"),
            ..unasserted
        };
        assert!(!row_identity_matches(&row, &disagreeing));

        // A captured `Some` that agrees still matches.
        let agreeing = RowIdentity {
            session_id: Some("session-abc"),
            ..unasserted
        };
        assert!(row_identity_matches(&row, &agreeing));
    }

    fn claude_row_then_absent(
        short_id: &'static str,
        state: &'static str,
    ) -> impl Fn() -> crate::claude_roster::ClaudeAgentsSnapshot {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        move || {
            if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new(short_id, Some(state)),
                ])
            } else {
                crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn rm_cascades_claude_before_removing_the_registry_row() {
        let home = short_home("rmclaude");
        let row = claude_rm_row(
            "stopped-worker",
            "aaaa1111",
            "aaaa1111-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
        let called = std::sync::Mutex::new(Vec::new());
        let snapshots = claude_row_then_absent("aaaa1111", "stopped");

        let response = handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &|short_id| {
                called.lock().unwrap().push(short_id.to_string());
                Ok(())
            },
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(called.into_inner().unwrap(), vec!["aaaa1111"]);
        assert_eq!(response.result().unwrap()["harness_removed"], true);
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_cleans_a_crowned_rows_scope_manifest_best_effort() {
        let home = short_home("rmcrownstate");
        let project = home.root().join("project");
        let manifest = project.join(".fno/kings/alpha.md");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "---\nscope: alpha\n---\n").unwrap();
        let mut row = claude_rm_row(
            "stopped-worker",
            "aaaa2222",
            "aaaa2222-1111-2222-3333-444444444444",
        );
        row.cwd = project.to_string_lossy().into_owned();
        row.crown_level = Some(1);
        row.crown_scope = Some("alpha".into());
        row.crown_grantor = Some("human".into());
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
        let snapshots = claude_row_then_absent("aaaa2222", "stopped");

        let response = handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &|_| Ok(()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response.error().is_none(), "{response:?}");
        assert!(
            !manifest.exists(),
            "successful rm left crown loop state behind"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_never_deletes_a_successors_re_armed_manifest() {
        // The vacated row is removed with a manifest on disk naming a
        // DIFFERENT session: a successor crowned over the scope after this
        // row went terminal re-armed it. Deleting that file would disarm the
        // live successor's stop gate.
        let home = short_home("rmcrownsucc");
        let project = home.root().join("project");
        let manifest = project.join(".fno/kings/alpha.md");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            "---\nscope: alpha\nharness_session_id: bbbb9999-9999-4999-8999-999999999999\n---\n",
        )
        .unwrap();
        let mut row = claude_rm_row(
            "stopped-worker",
            "aaaa3333",
            "aaaa3333-1111-2222-3333-444444444444",
        );
        row.cwd = project.to_string_lossy().into_owned();
        row.crown_level = Some(1);
        row.crown_scope = Some("alpha".into());
        row.crown_grantor = Some("human".into());
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
        let snapshots = claude_row_then_absent("aaaa3333", "stopped");

        let response = handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &|_| Ok(()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response.error().is_none(), "{response:?}");
        assert!(
            manifest.exists(),
            "rm deleted a manifest naming a different session: the live successor's gate"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_requires_the_post_list_to_prove_the_claude_row_is_gone() {
        let home = short_home("rmpostlist");
        let row = claude_rm_row(
            "stopped-worker",
            "aaabbb11",
            "aaabbb11-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("aaabbb11", Some("stopped")),
                ])
            },
            &|_| Ok(()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response
            .error()
            .unwrap()
            .message
            .contains("survives successful claude rm"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_treats_a_positive_row_in_a_partial_post_list_as_surviving() {
        let home = short_home("rmpartialpost");
        let row = claude_rm_row(
            "stopped-worker",
            "aaabbb12",
            "aaabbb12-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
        let response = handle_rm_with(
            &ctx,
            &request,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::Unknown {
                rows: vec![crate::claude_roster::ClaudeAgentRow::new(
                    "aaabbb12",
                    Some("stopped"),
                )],
                warnings: vec!["one malformed row".into()],
            },
            &|_| Ok(()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response
            .error()
            .unwrap()
            .message
            .contains("survives successful claude rm"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_mux_failure_names_the_claude_side_already_removed() {
        let home = short_home("rmpartial");
        let mut row = claude_rm_row(
            "pane-worker",
            "aaaccc22",
            "aaaccc22-1111-2222-3333-444444444444",
        );
        row.short_id.clear();
        row.mux = Some(state::MuxRef {
            session: "work".into(),
            pane_id: 24,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "pane-worker"}));
        let snapshots = claude_row_then_absent("aaaccc22", "stopped");

        let response = handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &|_| Ok(()),
            &|_, _| Err("permission denied".into()),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let message = &response.error().unwrap().message;
        assert!(message.contains("claude harness row aaaccc22 removed"));
        assert!(message.contains("registry retained"));
        assert!(message.contains("mux pane work:24"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_keeps_registry_row_when_claude_refuses_without_force() {
        let home = short_home("rmrefuse");
        let row = claude_rm_row(
            "stopped-worker",
            "bbbb2222",
            "bbbb2222-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("bbbb2222", Some("stopped")),
                ])
            },
            &|_| Err("claude rm exited 1".into()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response
            .error()
            .unwrap()
            .message
            .contains("claude rm exited 1"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );

        let forced_request = Request::new(
            2,
            "agent.rm",
            json!({"name": "stopped-worker", "force": true}),
        );
        let forced_response = handle_rm_with(
            &ctx,
            &forced_request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("bbbb2222", Some("stopped")),
                ])
            },
            &|_| Err("claude rm exited 1".into()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;
        assert_eq!(forced_response.result().unwrap()["harness_removed"], false);
        assert_eq!(
            forced_response.result().unwrap()["harness_reason"],
            "claude rm exited 1"
        );
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_removes_a_row_the_registry_actually_holds() {
        // AC1-HP: a plain removal reports removed:true and a re-read shows
        // zero rows for the name.
        let home = short_home("rmhappy");
        let row = ask_row("w1", Some("2020-01-01T00:00:00Z"));
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "w1"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| Ok(()),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(response.result().unwrap()["removed"], true);
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// A temp dir whose `.git` is a FILE: linked-worktree shape, no real repo
    /// behind it (the gate and the removal are injected, so none is needed).
    fn fake_worktree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fno-rm-wt-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /elsewhere/worktrees/x.git\n").unwrap();
        dir
    }

    #[test]
    fn output_with_timeout_bounds_a_stalled_subprocess() {
        // The rm path's budget: a fast child answers, a stalled one is
        // bounded instead of parking the daemon's rm handler.
        let fast = output_with_timeout(
            {
                let mut c = std::process::Command::new("echo");
                c.arg("ok");
                c
            },
            10,
        );
        assert!(fast.is_some(), "a fast child answers");
        let stalled = output_with_timeout(
            {
                let mut c = std::process::Command::new("sleep");
                c.arg("30");
                c
            },
            1,
        );
        let stalled = stalled.expect("a stalled child is killed, not left running");
        assert!(
            !stalled.status.success(),
            "the killed child reads as a failed call"
        );
    }

    #[test]
    fn branch_merged_answers_real_repos() {
        // The rm door's half of the third bucket: a fresh branch is
        // unmerged; a fast-forward into the main line flips it.
        let root = std::env::temp_dir().join(format!("fno-rm-mg-{}", std::process::id()));
        let repo = root.join("repo");
        let wt = root.join("leaf");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &std::path::Path| {
            crate::git_test_helpers::git_run(args, cwd).unwrap()
        };
        let commit_args = [
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "seed",
        ];
        assert!(git(&["init", "-q", "-b", "main"], &repo).status.success());
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        assert!(git(&["add", "-A"], &repo).status.success());
        assert!(git(&commit_args, &repo).status.success());
        let worktree_add = [
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "feature",
        ];
        assert!(git(&worktree_add, &repo).status.success());
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        assert!(git(&["add", "-A"], &wt).status.success());
        assert!(git(&commit_args, &wt).status.success());

        assert_eq!(
            branch_merged(wt.to_str().unwrap()),
            Some(false),
            "an unmerged branch blocks the rm door"
        );

        assert!(git(&["merge", "-q", "feature"], &repo).status.success());
        assert_eq!(
            branch_merged(wt.to_str().unwrap()),
            Some(true),
            "a merged branch passes"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rm_take_worktree_removes_when_the_gate_answers_yes() {
        // AC5-HP: a clean tree goes. The branch survives by git's own
        // contract (`worktree remove` never deletes branches) - the command
        // choice is the sweep's, not a second policy.
        let wt = fake_worktree("clean");
        let mut row = ask_row("wt1", Some("2020-01-01T00:00:00Z"));
        row.cwd = wt.to_string_lossy().into_owned();
        let receipt = rm_take_worktree_with(&row, &|_| WorktreeGate::Reapable, &|_| Ok(()));
        assert_eq!(
            receipt.as_deref().map(|s| s.to_string()),
            Some(format!("worktree removed: {}", wt.to_string_lossy()))
        );
        std::fs::remove_dir_all(&wt).ok();
    }

    #[test]
    fn rm_take_worktree_keeps_a_blocked_tree_and_names_the_reason() {
        // AC6-EDGE: DIRTY or clean-and-unmerged keeps the tree; the receipt
        // names the path and the gate's reason. The ROW was already removed
        // by the caller - the receipt never blocks that.
        let wt = fake_worktree("dirty");
        let mut row = ask_row("wt2", Some("2020-01-01T00:00:00Z"));
        row.cwd = wt.to_string_lossy().into_owned();
        let receipt = rm_take_worktree_with(
            &row,
            &|_| WorktreeGate::Blocked("modified-tracked".into()),
            &|_| Ok(()),
        );
        assert_eq!(
            receipt.as_deref().map(|s| s.to_string()),
            Some(format!(
                "worktree kept: {} (the gate said no: modified-tracked)",
                wt.to_string_lossy()
            ))
        );
        std::fs::remove_dir_all(&wt).ok();
    }

    #[test]
    fn rm_take_worktree_keeps_the_tree_when_the_probe_cannot_answer() {
        // AC7-ERR: an unanswerable probe keeps the tree. Removal never guesses.
        let wt = fake_worktree("mute");
        let mut row = ask_row("wt3", Some("2020-01-01T00:00:00Z"));
        row.cwd = wt.to_string_lossy().into_owned();
        let receipt = rm_take_worktree_with(
            &row,
            &|_| WorktreeGate::Unanswerable("the reapable probe could not answer".into()),
            &|_| Ok(()),
        );
        assert_eq!(
            receipt.as_deref().map(|s| s.to_string()),
            Some(format!(
                "worktree kept: {} (the reapable probe could not answer)",
                wt.to_string_lossy()
            ))
        );
        std::fs::remove_dir_all(&wt).ok();
    }

    #[test]
    fn rm_take_worktree_is_a_noop_for_a_row_without_a_linked_worktree() {
        // A row that ran in a plain directory owns nothing removable: the
        // gate is never consulted, the receipt is None, nothing fails.
        let mut row = ask_row("wt4", Some("2020-01-01T00:00:00Z"));
        row.cwd = "/tmp/plain-cwd".into();
        let asked = std::cell::Cell::new(0);
        let receipt = rm_take_worktree_with(
            &row,
            &|_| {
                asked.set(asked.get() + 1);
                WorktreeGate::Reapable
            },
            &|_| Ok(()),
        );
        assert_eq!(receipt, None);
        assert_eq!(asked.get(), 0, "the gate was never consulted");
    }

    #[tokio::test]
    async fn rm_refuses_to_report_removed_when_the_row_is_already_gone() {
        // AC1-NEG (silent-no-op mode): the row entry_for_lifecycle resolved is no
        // longer in the file by the time the write runs (raced away by a
        // concurrent teardown, e.g. another rm or a direct `claude rm`). The
        // injected claude_rm closure deletes the row as its side effect,
        // reproducing that race deterministically: retain() then has nothing
        // to drop, and the handler must refuse rather than report removed:true.
        let home = short_home("rmalreadygone");
        let row = claude_rm_row(
            "raced-worker",
            "ccccdddd",
            "ccccdddd-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "raced-worker"}));
        let raced_home = home.clone();
        let snapshots = claude_row_then_absent("ccccdddd", "stopped");

        let response = handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &move |_| {
                state::update_registry(&raced_home.registry_json(), |registry| {
                    registry.entries.retain(|e| e.name != "raced-worker");
                })
                .unwrap();
                Ok(())
            },
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let message = &response.error().unwrap().message;
        assert!(message.contains("registry does not hold"));
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_keeps_the_audit_event_compact_when_diagnostics_are_oversized() {
        let home = short_home("rmeventoversize");
        let row = claude_rm_row(
            "stopped-worker",
            "bbbb2223",
            "bbbb2223-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(
            1,
            "agent.rm",
            json!({"name": "stopped-worker", "force": true}),
        );

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("bbbb2223", Some("stopped")),
                ])
            },
            &|_| Err("x".repeat(crate::events::MAX_EVENT_PAYLOAD_BYTES * 2)),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(response.result().unwrap()["event_written"], true);
        assert!(response.result().unwrap()["event_reason"].is_null());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_falls_back_to_the_session_uuid_prefix() {
        let home = short_home("rmfallback");
        let row = claude_rm_row("stopped-worker", "", "cccc3333-1111-2222-3333-444444444444");
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
        let called = std::sync::Mutex::new(Vec::new());
        let snapshots = claude_row_then_absent("cccc3333", "stopped");

        let response = handle_rm_with(
            &ctx,
            &request,
            &snapshots,
            &|short_id| {
                called.lock().unwrap().push(short_id.to_string());
                Ok(())
            },
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response.result().is_some());
        assert_eq!(called.into_inner().unwrap(), vec!["cccc3333"]);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_refuses_blocked_claude_row_with_rotation_remedy() {
        let home = short_home("rmblocked");
        let mut row = claude_rm_row(
            "blocked-worker",
            "dddd4444",
            "dddd4444-1111-2222-3333-444444444444",
        );
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(
            1,
            "agent.rm",
            json!({"name": "blocked-worker", "force": true}),
        );

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("dddd4444", Some("blocked")),
                ])
            },
            &|_| panic!("blocked row must not reach claude rm"),
            &|_, _| panic!("blocked row must not reach mux kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let message = &response.error().unwrap().message;
        assert!(message.contains("rotate"));
        assert!(!message.contains("--force"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_accepts_terminal_claude_rows_that_remain_in_the_roster() {
        for (index, state) in ["done", "stopped", "failed"].into_iter().enumerate() {
            let home = short_home(&format!("rmterminal{state}"));
            let short_id = format!("dead{index:04}");
            let session_id = format!("{short_id}-1111-2222-3333-444444444444");
            let mut row = claude_rm_row("finished-worker", &short_id, &session_id);
            row.status = AgentStatus::Live;
            state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
                .unwrap();
            let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
            let request = Request::new(1, "agent.rm", json!({"name": "finished-worker"}));
            let first = std::sync::atomic::AtomicBool::new(true);

            let response = handle_rm_with(
                &ctx,
                &request,
                &|| {
                    if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                            crate::claude_roster::ClaudeAgentRow::new(&short_id, Some(state)),
                        ])
                    } else {
                        crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new())
                    }
                },
                &|_| Ok(()),
                &|_, _| Ok(true),
                &|_, _| PaneProbe::Unknown,
            )
            .await;

            assert_eq!(response.result().unwrap()["removed"], true, "state={state}");
            assert!(state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .is_empty());
            std::fs::remove_dir_all(home.root()).ok();
        }
    }

    #[tokio::test]
    async fn rm_still_refuses_an_idle_claude_row() {
        let home = short_home("rmidle");
        let mut row = claude_rm_row(
            "idle-worker",
            "1d1e0001",
            "1d1e0001-1111-2222-3333-444444444444",
        );
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "idle-worker"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("1d1e0001", Some("idle")),
                ])
            },
            &|_| panic!("an idle row must not reach claude rm"),
            &|_, _| panic!("an idle row must not reach mux kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response.error().unwrap().message.contains("still live"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_unknown_claude_list_cascades_but_reports_unverified() {
        let home = short_home("rmunverified");
        let row = claude_rm_row(
            "stopped-worker",
            "eeee5555",
            "eeee5555-1111-2222-3333-444444444444",
        );
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stopped-worker"}));
        let called = std::sync::atomic::AtomicBool::new(false);

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::unknown("list timed out"),
            &|_| {
                called.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(called.load(std::sync::atomic::Ordering::Relaxed));
        assert!(response.result().unwrap()["harness_removed"].is_null());
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_removes_a_stored_live_row_provably_gone_from_the_roster() {
        // AC2-HP (false-refusal mode): a row torn down by hand with `claude
        // stop`/`claude rm` never gets AgentStatus::Live written back. The
        // live gate must reconcile with the roster, not the stored enum.
        let home = short_home("rmprovengone");
        let mut row = claude_rm_row(
            "hand-torn-down",
            "ffff6666",
            "ffff6666-1111-2222-3333-444444444444",
        );
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "hand-torn-down"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| panic!("row already absent from the roster must not reach claude rm"),
            &|_, _| Ok(true),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(response.result().unwrap()["removed"], true);
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_refuses_a_stored_live_row_when_the_roster_is_unknown() {
        // AC2-EDGE: an Unknown snapshot (the shellout failed, timed out, or
        // parsed badly) is not proof of anything; keep refusing.
        let home = short_home("rmrosterunknown");
        let mut row = claude_rm_row(
            "maybe-live",
            "aaaa9999",
            "aaaa9999-1111-2222-3333-444444444444",
        );
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "maybe-live"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::unknown("list timed out"),
            &|_| panic!("an unknown roster must not reach claude rm"),
            &|_, _| panic!("an unknown roster must not reach mux kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert!(response.error().is_some());
        {
            // Positive markers (x-d19e): the unprovable case names the retry,
            // states what forcing costs, and offers no override flag.
            let message = &response.error().unwrap().message;
            assert!(
                message.contains("Retry once the roster is readable"),
                "{}",
                message
            );
            assert!(message.contains("resume handle"), "{}", message);
            assert!(!message.contains("--force"), "{}", message);
        }
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_refuses_a_row_the_roster_still_carries_and_names_no_force() {
        // AC2-NEG + AC2-COV: the short id IS present in a known snapshot, so
        // the row really is live. The refusal names the working incantation
        // (claude takes the short id, not the agent name) and never --force,
        // which a king previously read as the remedy and applied to five
        // genuinely-live rows.
        let home = short_home("rmstilllive");
        let mut row = claude_rm_row(
            "genuinely-live",
            "bbbb8888",
            "bbbb8888-1111-2222-3333-444444444444",
        );
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "genuinely-live"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| {
                crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
                    crate::claude_roster::ClaudeAgentRow::new("bbbb8888", Some("running")),
                ])
            },
            &|_| panic!("a genuinely live row must not reach claude rm"),
            &|_, _| panic!("a genuinely live row must not reach mux kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let message = &response.error().unwrap().message;
        assert!(message.contains("claude agents --json --all"));
        assert!(message.contains("fno agents stop"));
        // Specimen guard (x-d19e): this branch is the landed bar - safe verb,
        // self-proceeding command, and the hand-teardown cost named. A rewrite
        // that drops any of the three must fail here, not in a king's reign.
        assert!(message.contains("rm proceeds on its own"), "{}", message);
        assert!(message.contains("spends the resume handle"), "{}", message);
        // This refusal used to offer `claude stop <row>` then `claude rm <row>`
        // as a by-hand alternative, and this test required it. Ruling
        // d-1900e419 retired that pair: the harness row IS the resume handle,
        // and dropping it by hand spends the handle for nothing rm has not
        // already done. The refusal must not teach it back.
        // retired-ok: asserts the retired pair is ABSENT from the refusal.
        assert!(!message.contains("claude stop bbbb8888"));
        // retired-ok: asserts the retired pair is ABSENT from the refusal.
        assert!(!message.contains("claude rm bbbb8888"));
        assert!(!message.contains("--force"));
        assert!(!message.contains("-F"));
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_warns_that_forcing_orphans_a_live_process_and_the_text_is_pinned() {
        // x-ad13 ships the row/worktree guard split; the epic pins rm's own
        // live-process warning as a refusal that must survive it unchanged.
        // This is the non-claude arm's wording (the claude arm is pinned by
        // `rm_refuses_a_row_the_roster_still_carries_and_names_no_force`):
        // the refusal warns what forcing would do and never suggests it.
        let home = short_home("rmorphancodex");
        let mut row = ask_row("live-codex", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("codex".into());
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "live-codex"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| panic!("a non-claude row never reads the claude roster"),
            &|_| panic!("a live row must not reach claude rm"),
            &|_, _| panic!("a live row must not reach mux kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let message = &response.error().unwrap().message;
        assert!(
            message.contains("Forcing it through orphans a live process"),
            "{}",
            message
        );
        assert!(
            message.contains("fno agents stop live-codex"),
            "{}",
            message
        );
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_refusal_on_a_claude_row_without_a_row_id_names_stop_not_force() {
        // x-d19e: the no-row-id arm cannot check the roster, but the refusal
        // still owes the reader the safe verb and the cost of forcing through,
        // never the override flag itself.
        let home = short_home("rmnorowid");
        // short_id empty AND session id empty (a pid carries the handle
        // invariant instead): claude_row_id answers None, which is the arm
        // where the roster can never be consulted.
        let mut row = ask_row("idless-live", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("claude".into());
        row.harness_session_id = None;
        row.pid = Some(4242);
        row.pid_start_time = Some(123456);
        row.status = AgentStatus::Live;
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "idless-live"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| panic!("an unresolvable row must not reach claude rm"),
            &|_, _| panic!("an unresolvable row must not reach mux kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let message = &response.error().expect("still live must refuse").message;
        assert!(
            message.contains("no resolvable harness row id"),
            "{}",
            message
        );
        assert!(
            message.contains("fno agents stop idless-live"),
            "{}",
            message
        );
        assert!(message.contains("rm proceeds on its own"), "{}", message);
        assert!(message.contains("resume handle"), "{}", message);
        assert!(!message.contains("--force"), "{}", message);
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_kills_a_mux_pane_before_removing_its_registry_row() {
        let home = short_home("rmpane");
        let mut row = ask_row("pane-worker", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("gemini".into());
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 24,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "pane-worker"}));
        let killed = std::sync::Mutex::new(Vec::new());

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| panic!("non-Claude row must not read the Claude list"),
            &|_| panic!("non-Claude row must not call claude rm"),
            &|session, pane_id| {
                killed.lock().unwrap().push((session.to_string(), pane_id));
                Ok(true)
            },
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(killed.into_inner().unwrap(), vec![("main".to_string(), 24)]);
        assert_eq!(response.result().unwrap()["pane_removed"], true);
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_clears_a_stale_registry_row_after_the_mux_pane_is_already_absent() {
        let home = short_home("rmmissingpane");
        let mut row = ask_row("stale-pane-worker", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("gemini".into());
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 24,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "stale-pane-worker"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| panic!("non-Claude row must not read the Claude list"),
            &|_| panic!("non-Claude row must not call claude rm"),
            &|_, _| Ok(false),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        assert_eq!(response.result().unwrap()["pane_removed"], false);
        assert_eq!(
            response.result().unwrap()["pane_reason"],
            "mux pane already absent"
        );
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_clears_a_stored_live_pane_row_whose_pane_is_provably_absent() {
        // The fleet-reap deadlock: the row still reads live while the pane it
        // names is gone. The gate must test the referent, so the probe's
        // Absent verdict clears the row without --force.
        let home = short_home("rmpanegone");
        let mut row = ask_row("dead-pane-worker", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("opencode".into());
        row.status = AgentStatus::Live;
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 76,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "dead-pane-worker"}));
        let probed = std::sync::Mutex::new(Vec::new());

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| panic!("non-Claude row must not read the Claude list"),
            &|_| panic!("non-Claude row must not call claude rm"),
            &|session, pane_id| {
                assert_eq!((session, pane_id), ("main", 76));
                Ok(false)
            },
            &|session, pane_id| {
                probed.lock().unwrap().push((session.to_string(), pane_id));
                PaneProbe::Absent
            },
        )
        .await;

        assert_eq!(probed.into_inner().unwrap(), vec![("main".to_string(), 76)]);
        assert_eq!(response.result().unwrap()["removed"], true);
        assert_eq!(response.result().unwrap()["pane_removed"], false);
        assert_eq!(
            response.result().unwrap()["pane_reason"],
            "mux pane already absent"
        );
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_still_refuses_a_stored_live_pane_row_when_the_probe_is_unknown() {
        // Fail-closed: a probe that errored, timed out, or parsed badly proves
        // nothing. The refusal and the row both stay.
        let home = short_home("rmpaneunknown");
        let mut row = ask_row("maybe-pane-worker", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("opencode".into());
        row.status = AgentStatus::Live;
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 76,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "maybe-pane-worker"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| panic!("non-Claude row must not read the Claude list"),
            &|_| panic!("non-Claude row must not call claude rm"),
            &|_, _| panic!("a refused row must not reach the pane kill"),
            &|_, _| PaneProbe::Unknown,
        )
        .await;

        let error = response.error().expect("a stored-live row must be refused");
        assert!(error.message.contains("still live"), "{}", error.message);
        // Positive markers (x-d19e): the refusal names the safe verb and the
        // cost of forcing past it; the override lives in --help, never here.
        assert!(
            error.message.contains("fno agents stop maybe-pane-worker"),
            "{}",
            error.message
        );
        assert!(error.message.contains("resume handle"), "{}", error.message);
        assert!(!error.message.contains("--force"), "{}", error.message);
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn rm_still_refuses_a_stored_live_pane_row_when_the_pane_is_present() {
        // A live pane is a live worker; the refusal must stand.
        let home = short_home("rmpanepresent");
        let mut row = ask_row("live-pane-worker", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("opencode".into());
        row.status = AgentStatus::Live;
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 76,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.rm", json!({"name": "live-pane-worker"}));

        let response = handle_rm_with(
            &ctx,
            &request,
            &|| panic!("non-Claude row must not read the Claude list"),
            &|_| panic!("non-Claude row must not call claude rm"),
            &|_, _| panic!("a refused row must not reach the pane kill"),
            &|_, _| PaneProbe::Present,
        )
        .await;

        let error = response.error().expect("a stored-live row must be refused");
        assert!(error.message.contains("still live"), "{}", error.message);
        // Positive markers (x-d19e): same contract as the probe-unknown arm.
        assert!(
            error.message.contains("fno agents stop live-pane-worker"),
            "{}",
            error.message
        );
        assert!(error.message.contains("resume handle"), "{}", error.message);
        assert!(!error.message.contains("--force"), "{}", error.message);
        assert_eq!(
            state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .len(),
            1
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test]
    async fn stop_refusal_names_a_pane_kill_the_mux_parser_accepts() {
        // The refusal string and the parser drift independently: the refusal
        // once printed `main:76` while the parser demanded a bare number, so
        // the instrument named a way out that errored with EXIT_USAGE. Hold
        // both sides in one test: extract the command this handler really
        // printed and feed it to the real parse_pane_args, no hardcoded
        // expected string anywhere.
        let home = short_home("stoprefusal");
        let mut row = ask_row("pane-worker", Some("2020-01-01T00:00:00Z"));
        row.harness = Some("opencode".into());
        row.status = AgentStatus::Live;
        row.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 76,
        });
        state::update_registry(&home.registry_json(), |registry| registry.entries.push(row))
            .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let request = Request::new(1, "agent.stop", json!({"name": "pane-worker"}));

        let response = handle_stop(&ctx, &request).await;

        let error = response.error().expect("a pane row must be refused");
        let printed = error
            .message
            .split("Kill the pane: `")
            .nth(1)
            .expect("refusal names the kill command")
            .split('`')
            .next()
            .expect("the printed command is backtick-closed");
        let selector = printed
            .strip_prefix("fno mux pane kill ")
            .expect("the printed command is the pane kill verb");
        let args: Vec<std::ffi::OsString> = vec!["kill".into(), selector.into()];
        let parsed =
            fno::mux_cli::parse_pane_args(&args).expect("the refusal's own command must parse");
        assert_eq!(parsed.session.as_deref(), Some("main"));
        assert_eq!(parsed.cmd, fno::mux_cli::PaneCmd::Kill { pane: 76 });
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn mux_missing_pane_receipt_is_idempotent_absence() {
        assert!(mux_pane_is_absent("fno mux: no such pane: 24"));
        assert!(mux_pane_is_absent(
            "cannot reach session main: No such file or directory (os error 2)"
        ));
        assert!(!mux_pane_is_absent("mux configuration not found"));
        assert!(!mux_pane_is_absent("fno mux: permission denied"));
    }

    /// The real summary line, copied from this machine's output.
    const REAL_SUMMARY: &str = "would-archive      feature/x-3e17   /some/wt\n\
Summary: 12 would archive, 37 kept (19 unmerged, 11 unpushed, 5 dirty, 0 live-session, 1 processes, 0 salvage-failed, 0 needs-confirmation, 1 app-owned, 1 permanent), 0 failed  [dry-run: no changes made; pass --apply to execute]\n";

    #[test]
    fn sweep_summary_parses_the_real_line() {
        let r = parse_worktree_sweep(REAL_SUMMARY).expect("parses");
        assert_eq!(r.eligible, 12);
        assert_eq!(r.kept, 37);
        assert_eq!(r.dirty, 5);
    }

    #[test]
    fn sweep_summary_parses_the_apply_mode_line() {
        // The apply pass says "archived", not "would archive"; the eligible
        // count must read from whichever verb the line carries.
        let line = "archived         feature/x-3e17   /some/wt\n\
Summary: 3 archived, 4 kept (1 unmerged, 1 unpushed, 1 dirty), 0 failed\n";
        let r = parse_worktree_sweep(line).expect("parses");
        assert_eq!(r.eligible, 3);
        assert_eq!(r.kept, 4);
        assert_eq!(r.dirty, 1);
    }

    #[test]
    fn sweep_summary_absent_is_none_not_zero() {
        // A zeroed report is indistinguishable from a clean machine. An absence
        // has two explanations and only a real reading may produce a count.
        assert!(parse_worktree_sweep("").is_none());
        assert!(parse_worktree_sweep("some other output\n").is_none());
    }

    #[test]
    fn sweep_reports_every_repo_including_the_quiet_ones() {
        let home = tmp_home("wt-sweep-quiet");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let quiet =
            "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into(), "/repo/b".into()],
            &|_| false.into(),
            &|_, _| WorktreeSweepOutput {
                exit_code: Some(0),
                stdout: quiet.into(),
                stderr: String::new(),
            },
        );

        assert_eq!(swept, 2, "a tick that finds nothing must still report");
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert_eq!(log.matches("worktree_sweep").count(), 2);
        assert!(log.contains("report-only"));
        assert!(!log.contains("apply-orders"));
    }

    #[test]
    fn sweep_applies_only_when_a_reap_order_stands() {
        // Ruling preserved: a merged PR is proof, a timer tick is not. The
        // timer lane applies ONLY when the merge ritual minted an order.
        let home = tmp_home("wt-sweep-ordered");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let quiet =
            "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| true.into(),
            &|_, apply| {
                assert!(apply, "a standing order must reach the verb as --apply");
                WorktreeSweepOutput {
                    exit_code: Some(0),
                    stdout: quiet.into(),
                    stderr: String::new(),
                }
            },
        );

        assert_eq!(swept, 1);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("apply-orders"));
        assert!(!log.contains("report-only"));
    }

    #[test]
    fn sweep_reads_reap_orders_in_each_repository_scope() {
        let home = tmp_home("wt-sweep-repo-orders");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let seen = std::sync::Mutex::new(Vec::new());
        let quiet =
            "Summary: 0 would archive, 0 kept (0 unmerged, 0 unpushed, 0 dirty), 0 failed\n";

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into(), "/repo/b".into()],
            &|root| (root == "/repo/b").into(),
            &|root, apply| {
                seen.lock().unwrap().push((root.to_string(), apply));
                WorktreeSweepOutput {
                    exit_code: Some(0),
                    stdout: quiet.into(),
                    stderr: String::new(),
                }
            },
        );

        assert_eq!(swept, 2);
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![("/repo/a".into(), false), ("/repo/b".into(), true)]
        );
    }

    #[test]
    fn sweep_skips_a_repo_when_its_order_probe_is_unreadable() {
        let home = tmp_home("wt-sweep-order-unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let ran_cleanup = std::sync::atomic::AtomicBool::new(false);

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| WorktreeSweepOrderRead {
                standing: None,
                exit_code: Some(7),
                stderr: "claim store unreadable\nextra detail\n".into(),
            },
            &|_, _| {
                ran_cleanup.store(true, std::sync::atomic::Ordering::Relaxed);
                unreachable!("an unreadable order probe must skip cleanup")
            },
        );

        assert_eq!(swept, 0);
        assert!(!ran_cleanup.load(std::sync::atomic::Ordering::Relaxed));
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("\"error\":\"unreadable-orders\""));
        assert!(log.contains("\"exit_code\":7"));
        assert!(log.contains("\"stderr\":\"claim store unreadable\""));
        assert!(!log.contains("extra detail"));
        assert!(!log.contains("report-only"));
    }

    #[test]
    fn sweep_honours_its_own_6h_floor() {
        let home = tmp_home("wt-sweep-floor");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = |_: &str, _: bool| WorktreeSweepOutput {
            exit_code: Some(0),
            stdout: REAL_SUMMARY.into(),
            stderr: String::new(),
        };
        let now = 1_000_000;

        assert_eq!(
            worktree_sweep(
                &home,
                &emitter,
                now,
                &["/repo/a".into()],
                &|_| false.into(),
                &out,
            ),
            1
        );
        // Same window: skipped entirely, no second reading.
        assert_eq!(
            worktree_sweep(
                &home,
                &emitter,
                now + 60,
                &["/repo/a".into()],
                &|_| false.into(),
                &out
            ),
            0
        );
        // A little over six hours later: fires again.
        assert_eq!(
            worktree_sweep(
                &home,
                &emitter,
                now + 21_601,
                &["/repo/a".into()],
                &|_| false.into(),
                &out
            ),
            1
        );
    }

    #[test]
    fn sweep_never_passes_apply_on_its_own_authority() {
        // Ruling: a merged PR is proof, a timer tick is not. The fn body may
        // not carry an --apply literal: applying is decided by the injected
        // orders read (merge-minted claims), never by the sweep itself.
        let src = include_str!("daemon.rs");
        let idx = src
            .find("fn worktree_sweep(")
            .expect("worktree_sweep exists");
        let body = &src[idx..idx + 2000.min(src.len() - idx)];
        assert!(!body.contains("--apply"));
    }

    #[test]
    fn sweep_records_an_unreadable_summary_rather_than_inventing_zeros() {
        let home = tmp_home("wt-sweep-unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| false.into(),
            &|_, _| WorktreeSweepOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            },
        );

        assert_eq!(swept, 0);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("unreadable-summary"));
        assert!(!log.contains("\"eligible\""));
    }

    #[test]
    fn sweep_records_a_nonzero_exit_and_first_stderr_line() {
        let home = tmp_home("wt-sweep-nonzero");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let output = WorktreeSweepOutput {
            exit_code: Some(7),
            stdout: String::new(),
            stderr: "permission denied\nextra detail\n".into(),
        };

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| false.into(),
            &|_, _| output.clone(),
        );

        assert_eq!(swept, 0);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("\"exit_code\":7"));
        assert!(log.contains("\"stderr\":\"permission denied\""));
        assert!(!log.contains("extra detail"));
    }

    #[test]
    fn sweep_distinguishes_a_zero_exit_with_no_summary() {
        let home = tmp_home("wt-sweep-zero-no-summary");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        let swept = worktree_sweep(
            &home,
            &emitter,
            1_000_000,
            &["/repo/a".into()],
            &|_| false.into(),
            &|_, _| WorktreeSweepOutput {
                exit_code: Some(0),
                stdout: "no summary here\n".into(),
                stderr: String::new(),
            },
        );

        assert_eq!(swept, 0);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("\"exit_code\":0"));
        assert!(log.contains("\"stderr\":\"\""));
    }

    #[test]
    fn stale_summary_parses_the_asked_line() {
        // The verb's ACTUAL --json output, embedded summary text and all -
        // not a synthetic standalone Summary line the parser could never see.
        let line = r#"{"outcome": "asked", "question_id": "q-37222570", "stale_count": 12, "oldest_h": 1829, "summary": "Summary: 12 stale, outcome asked, oldest 1829h"}"#;
        let r = parse_stale_sweep(line).expect("parses");
        assert_eq!(r.stale, 12);
        assert_eq!(r.oldest_h, 1829);
        assert_eq!(r.outcome, "asked");
    }

    #[test]
    fn stale_summary_carries_the_refused_outcome_word() {
        // On the refused path the count is NOT a real reading, so the event
        // must carry the word that says so instead of a fabricated zero.
        let line = r#"{"outcome": "refused", "question_id": "", "stale_count": 0, "oldest_h": 0, "summary": "Summary: 0 stale, outcome refused, oldest 0h"}"#;
        let r = parse_stale_sweep(line).expect("parses");
        assert_eq!(r.stale, 0);
        assert_eq!(r.outcome, "refused");
    }

    #[test]
    fn stale_summary_reads_duplicate_as_not_asked() {
        // A duplicate no-op is a real reading, not silence: the event must
        // carry the measured set even when the fold asked nothing.
        let line = r#"{"outcome": "duplicate", "question_id": "q-37222570", "stale_count": 12, "oldest_h": 1830, "summary": "Summary: 12 stale, outcome duplicate, oldest 1830h"}"#;
        let r = parse_stale_sweep(line).expect("parses");
        assert_eq!(r.stale, 12);
        assert_eq!(r.outcome, "duplicate");
    }

    #[test]
    fn stale_parser_refuses_the_text_mode_line() {
        // If the verb ever regresses to text-only output, the parser must
        // answer None (loud error event), never misread the embedded
        // summary text as a reading.
        assert!(parse_stale_sweep("Summary: 12 stale, outcome asked, oldest 1829h\n").is_none());
    }

    #[test]
    fn stale_summary_absent_is_none_not_zero() {
        // A zeroed report is indistinguishable from a clean machine. An
        // absence has two explanations and only a real reading produces a
        // count.
        assert!(parse_stale_sweep("").is_none());
        assert!(parse_stale_sweep("some other output\n").is_none());
    }

    #[test]
    fn stale_sweep_honours_its_own_6h_floor() {
        let home = tmp_home("stale-sweep-floor");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = || {
            Some(
                r#"{"outcome": "asked", "question_id": "q-aa", "stale_count": 1, "oldest_h": 30, "summary": "Summary: 1 stale, outcome asked, oldest 30h"}"#
                    .to_string(),
            )
        };
        let now = 1_000_000;

        assert_eq!(stale_sweep(&home, &emitter, now, &out), 1);
        // Within the floor: skipped entirely, no second reading.
        assert_eq!(stale_sweep(&home, &emitter, now + 60, &out), 0);
        // Past the floor: fires again.
        assert_eq!(
            stale_sweep(&home, &emitter, now + STALE_SWEEP_INTERVAL_SECS + 1, &out),
            1
        );
    }

    #[test]
    fn stale_sweep_emits_on_a_quiet_run() {
        // A tick that stays silent when it finds nothing cannot be told from
        // a tick that never ran, and this lane exists precisely to prove the
        // sweep fires at all: outcome none still emits.
        let home = tmp_home("stale-sweep-quiet");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = || {
            Some(
                r#"{"outcome": "none", "question_id": "", "stale_count": 0, "oldest_h": 0, "summary": "Summary: 0 stale, outcome none, oldest 0h"}"#
                    .to_string(),
            )
        };

        assert_eq!(stale_sweep(&home, &emitter, 1_000_000, &out), 1);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("stale_sweep"));
        assert!(log.contains("\"stale_count\":0"));
    }

    #[test]
    fn stale_sweep_records_an_unreadable_summary_rather_than_inventing_zeros() {
        let home = tmp_home("stale-sweep-unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        assert_eq!(stale_sweep(&home, &emitter, 1_000_000, &|| None), 0);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("unreadable-summary"));
        assert!(!log.contains("\"stale_count\""));
    }

    #[test]
    fn stale_sweep_takes_no_apply_form() {
        // The lane routes information and changes no removal path: the fn
        // body may not carry an apply decision at all.
        let src = include_str!("daemon.rs");
        let idx = src.find("fn stale_sweep(").expect("stale_sweep exists");
        let body = &src[idx..idx + 2000.min(src.len() - idx)];
        assert!(!body.contains("--apply"));
    }

    #[test]
    fn linked_worktree_detection_separates_owners_from_passers_through() {
        // The whole ownership test is `.git` being a FILE (a `gitdir:` pointer)
        // rather than a directory. Getting it backwards would either pin every
        // row again or reap rows that DO own a dirty worktree.
        let dir = tempfile::tempdir().expect("tmpdir");
        let base = dir.path();

        let canonical = base.join("canonical");
        std::fs::create_dir_all(canonical.join(".git")).unwrap();
        assert!(
            !is_linked_worktree(canonical.to_str().unwrap()),
            "a .git DIRECTORY is the canonical checkout; the row owns nothing removable"
        );

        let linked = base.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /somewhere/.git/worktrees/x\n").unwrap();
        assert!(
            is_linked_worktree(linked.to_str().unwrap()),
            "a .git FILE is a linked worktree; cleanliness decides its row"
        );

        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!is_linked_worktree(plain.to_str().unwrap()));

        // Unreadable and empty both fail closed toward "owns nothing", so the row
        // is judged on terminal status and grace instead of waiting forever on a
        // cleanliness answer that can never arrive.
        assert!(!is_linked_worktree(""));
        assert!(!is_linked_worktree("/nonexistent/path/that/cannot/be/read"));
    }

    #[test]
    fn gc_sweep_empty_registry_is_noop() {
        let home = tmp_home("gc-empty");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let summary = gc_sweep(&home, &emitter, 900, 7);
        assert!(summary.retired.is_empty());
        assert!(summary.pruned.is_empty());
    }

    // The gc ladder, reap-receipt gate and plan_reconcile
    // families, moved verbatim into their own module (file budget: this
    // file is far over the shrink-only line; test motion is the sanctioned
    // shrink).
    #[path = "gc_receipts.rs"]
    mod gc_receipts;

    // --- plan_reconcile (US6.9): tri-state, status-aware transitions, budget ---

    fn rentry(name: &str, status: AgentStatus, last_reconciled: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: name.into(),
            short_id: name.into(),
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: Some("sid".into()),
            spawn_trigger: None,
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
            keeper_child_pid: None,
            log_path: None,
            last_reconciled_at: last_reconciled.map(String::from),
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            sandbox_posture: None,
            ..Default::default()
        }
    }

    fn probe_err() -> crate::provider::ReachabilityProbeError {
        crate::provider::ReachabilityProbeError::new("codex", "store unavailable")
    }

    // --- find_uuid_backfill_row (x-c393): backfill a null-uuid bg row ---------

    /// A `claude --bg` row: jobId in `short_id`, `claude_session_uuid` null.
    /// A transcript file for a fixture row that is genuinely finished.
    ///
    /// The corroboration gate needs a POSITIVE reading that a worker stopped
    /// writing, and these tests run with a zero grace window, so any file that
    /// already exists reads as stale. A fixture with no transcript at all reads
    /// as UNKNOWN and is kept, which is the correct production behaviour and
    /// would silently hollow out these assertions.
    fn stale_log(dir: &std::path::Path) -> String {
        let p = dir.join("transcript.jsonl");
        std::fs::write(&p, "{}\n").unwrap();
        // BACKDATE IT. A file written this second reads as fresh even against a
        // zero-length window, so a fixture that only creates the file proves the
        // opposite of what it claims.
        assert!(std::process::Command::new("touch")
            .args(["-t", "200001010000", &p.to_string_lossy()])
            .status()
            .expect("touch runs")
            .success());
        p.to_string_lossy().into_owned()
    }

    fn bg_claude_row(name: &str, short_id: &str) -> RegistryEntry {
        let mut e = rentry(name, AgentStatus::Live, None);
        e.legacy_provider = "claude".into();
        e.short_id = short_id.into();
        e.claude_session_uuid = None;
        // A row fno itself spawned: the retirement origin gate retires only
        // these, so every retirement-path fixture starts from spawn.
        e.origin = Some("spawn".into());
        // x-7bcd: needs a resolvable handle; short_id is the transport key
        // this row is actually tested against, not one of the three legs.
        e.log_path = Some(format!("/tmp/{name}.log"));
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
        row.legacy_provider = "codex".into();
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
        let dup_row = || -> RegistryEntry {
            let mut e = rentry("dup", AgentStatus::Live, None);
            e.log_path = Some("/tmp/dup.log".into()); // x-7bcd: resolvable handle
            e
        };
        assert!(reserve(dup_row()), "first wins");
        assert!(!reserve(dup_row()), "second loses the race -> no insert");
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
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
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

    /// A codex THREAD row: no short_id, interactive host mode, full session id,
    /// and a recorded rollout path (the durable resume object).
    fn thread_entry(name: &str, status: AgentStatus, log_path: Option<String>) -> RegistryEntry {
        let mut entry = rentry(name, status, None);
        entry.pid = Some(999_999_999);
        entry.short_id = String::new();
        entry.legacy_provider = String::new();
        entry.harness = Some("codex".into());
        entry.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.into());
        entry.session_id = None;
        entry.harness_session_id = Some(format!("0198thread-{name}-00000000000000"));
        entry.log_path = log_path;
        entry
    }

    #[test]
    fn reconcile_leaves_a_hosted_codex_thread_untouched() {
        let entries = vec![thread_entry(
            "t-hosted",
            AgentStatus::Live,
            Some("/tmp/r.jsonl".into()),
        )];
        let (changes, _) = plan_reconcile(
            &entries,
            |_| Ok(false),
            || false,
            |_| true,
            |_| false,
            |_| true,               // thread_hosted: the daemon map names this row
            |_| false,              // rollout_exists (irrelevant while hosted)
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(
            changes[0].new_status, None,
            "a hosted thread is the daemon's own; the stale pid must not settle it"
        );
    }

    #[test]
    fn reconcile_settles_an_unhosted_thread_with_a_rollout_to_orphaned() {
        let entries = vec![thread_entry(
            "t-resumable",
            AgentStatus::Live,
            Some("/tmp/r.jsonl".into()),
        )];
        let (changes, _) = plan_reconcile(
            &entries,
            |_| Ok(false),
            || false,
            |_| true,
            |_| false,
            |_| false, // not hosted: the actor is gone (daemon restart, resume failed)
            |_| true,  // the rollout file exists: the durable object survives
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,      // roster readable: the flip needs a successful roster read
        );
        assert_eq!(
            changes[0].new_status,
            Some(AgentStatus::Orphaned),
            "resumable thread reads Orphaned, never Live-forever"
        );
    }

    /// AC15: a row whose startup resume FAILED reads Orphaned after the
    /// recovery pass, never Live-forever. The resume is made to fail
    /// deterministically via a nonexistent cwd (app-server spawn cannot even
    /// start there).
    /// AC11: a yolo spawn stamps the posture on the row; the resume lane's
    /// helper reads it back.
    ///
    /// It drives a fake SHARED daemon. It used to install a stdio `codex` on
    /// PATH and let the driver fork it. After the transport moved to the
    /// shared daemon that fake was never reached: on a developer machine the
    /// driver connected to the operator's REAL daemon and the test passed by
    /// starting a real thread, and in CI, where no daemon runs, it panicked.
    /// A test that reaches a live daemon is not a unit test, so it takes the
    /// same fake every other one here does.
    #[test]
    fn build_codex_thread_entry_stamps_the_launch_posture() {
        let worktree = tempfile::tempdir().unwrap();
        let _guard = crate::path_test_guard();
        let start = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                // The fake must outlive the start: it owns CODEX_HOME.
                let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
                    crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-p"),
                );
                crate::codex_thread::CodexThread::start(worktree.path(), None, true, None)
                    .await
                    .expect("yolo thread starts")
            });
        let yolo = build_codex_thread_entry("t", worktree.path(), &start, None, None, true, None);
        assert_eq!(yolo.sandbox_posture.as_deref(), Some("danger-full-access"));
        assert!(
            entry_posture_is_full_access(&yolo)
                && yolo.fno_id.as_deref() == Some("thread-p")
                && yolo.mux.is_none()
        );
        let bounded =
            build_codex_thread_entry("t", worktree.path(), &start, None, None, false, None);
        assert_eq!(bounded.sandbox_posture.as_deref(), Some("workspace-write"));
        assert!(!entry_posture_is_full_access(&bounded));
        // A requested model stamps its basis on the row; an absent one
        // leaves the basis absent with it.
        let modeled = build_codex_thread_entry(
            "t",
            worktree.path(),
            &start,
            Some("gpt-5.6-sol"),
            None,
            false,
            None,
        );
        assert_eq!(modeled.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(modeled.model_basis.as_deref(), Some("requested"));
        assert_eq!(bounded.model_basis, None);
        // v25 positive marker: the route identity the spawn actually used,
        // read back non-empty from the minted row - the provider-outage
        // collector refuses evidence on a row whose axes are absent, so an
        // all-None stamp here would keep every daemon codex thread blind.
        assert_eq!(modeled.route_provider_id.as_deref(), Some("openai"));
        assert_eq!(modeled.model_name.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(modeled.account_record_id.as_deref(), Some("default"));
    }

    #[test]
    fn build_codex_thread_entry_stamps_the_request_node() {
        let worktree = tempfile::tempdir().unwrap();
        let _guard = crate::path_test_guard();
        let start = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let _daemon = crate::codex_fake_daemon::FakeDaemon::start(
                    crate::codex_fake_daemon::Behavior::quick().with_thread_id("thread-node"),
                );
                crate::codex_thread::CodexThread::start(worktree.path(), None, true, None)
                    .await
                    .expect("yolo thread starts")
            });
        let entry = build_codex_thread_entry(
            "t",
            worktree.path(),
            &start,
            None,
            None,
            true,
            Some("x-535c"),
        );
        assert_eq!(entry.node.as_deref(), Some("x-535c"));
    }

    /// AC12: a PRE-v19 row (no posture key) still parses and reads the safe
    /// default - never a parse failure, never an accidental escalation.
    #[test]
    fn pre_v19_row_without_posture_parses_with_the_safe_default() {
        let raw = json!({
            "name": "legacy-thread",
            "short_id": "",
            "legacy_provider": "",
            "harness": "codex",
            "harness_session_id": "0198old-0000-7000-8000-000000000001",
            "cwd": "/tmp",
            "project_root": "/tmp",
            "host_mode": "interactive",
            "status": "live",
            "created_at": "2026-08-01T00:00:00Z",
        });
        let entry: RegistryEntry = serde_json::from_value(raw).expect("pre-v19 row parses");
        assert_eq!(entry.sandbox_posture, None);
        assert!(
            !entry_posture_is_full_access(&entry),
            "an unrecorded posture reads the safe default, never full access"
        );
    }

    /// AC16: a codex PANE row (mux ref set) must refuse from the ask lane
    /// naming the pane verb, never reach ensure_codex_thread_handle and die
    /// with the confusing "is not a Codex thread".
    #[tokio::test(flavor = "current_thread")]
    async fn ask_a_codex_pane_row_refuses_naming_the_pane_verb() {
        let home = tmp_home("codex-pane-ask");
        state::update_registry(&home.registry_json(), |registry| {
            let mut entry = thread_entry("t-pane", AgentStatus::Live, None);
            entry.mux = Some(state::MuxRef {
                session: "main".into(),
                pane_id: 3,
            });
            entry.log_path = Some("/tmp/t-pane.log".into());
            registry.entries.push(entry);
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent"));
        let resp = handle_ask(
            &ctx,
            &Request::new(1, "agent.ask", json!({"name": "t-pane", "message": "hi"})),
        )
        .await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidStatus);
                assert!(
                    e.message.contains("pane worker") && e.message.contains("mux pane send"),
                    "refusal must name the pane verb: {}",
                    e.message
                );
                assert!(
                    !e.message.contains("is not a Codex thread"),
                    "the confusing thread refusal must not surface: {}",
                    e.message
                );
            }
            _ => panic!("a pane row must refuse, got: {resp:?}"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recovery_stamps_a_failed_codex_thread_resume_orphaned() {
        let home = tmp_home("codex-recover-orphaned");
        state::update_registry(&home.registry_json(), |registry| {
            let mut entry = thread_entry(
                "t-dead",
                AgentStatus::Live,
                Some("/tmp/t-dead.jsonl".into()),
            );
            entry.cwd = "/nonexistent-cwd-for-resume-failure".into();
            entry.project_root = entry.cwd.clone();
            entry.harness_session_id = Some("0198dead-0000-7000-8000-00000000000f".into());
            entry.codex_session_id = entry.harness_session_id.clone();
            entry.pid = None;
            registry.entries.push(entry);
        })
        .unwrap();
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
        recover_codex_threads(&ctx).await;
        let registry = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry readable");
        assert_eq!(
            registry.find("t-dead").map(|entry| entry.status),
            Some(AgentStatus::Orphaned),
            "a failed resume must settle the row Orphaned, not Live-forever"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn reconcile_settles_an_unhosted_thread_without_a_rollout_to_exited() {
        let entries = vec![thread_entry("t-gone", AgentStatus::Live, None)];
        let (changes, _) = plan_reconcile(
            &entries,
            |_| Ok(false),
            || false,
            |_| true,
            |_| false,
            |_| false,              // not hosted
            |_| false,              // no rollout: the thread never got far enough to persist
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(
            changes[0].new_status,
            Some(AgentStatus::Exited),
            "an unhosted thread with no rollout is gone, not Live-forever"
        );
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
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(false),
            || false,
            |_| true,
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
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
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
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
    fn reconcile_mux_pane_liveness_follows_the_pid_not_the_store() {
        // Codex P1/P2 (#603): a mux-hosted pane is PTY-governed, so on a
        // session-store miss a live pid keeps it Live and a dead pid reaps to
        // Exited. A pid-less pane (_lookup_child_pid best-effort miss) has no PTY
        // signal and must NOT be preserved -- pid_live maps None to true, so that
        // would keep a maybe-dead pane immortal; it defers to store liveness
        // (orphan) instead.
        let mk = |name: &str, pid: Option<u32>| {
            let mut e = rentry(name, AgentStatus::Live, None);
            e.mux = Some(crate::state::MuxRef {
                session: "main".into(),
                pane_id: 7,
            });
            e.pid = pid;
            e
        };
        let entries = vec![
            mk("live-pane", Some(4242)), // pid present + alive
            mk("dead-pane", Some(4243)), // pid present + dead
            mk("pidless-pane", None),    // pid capture missed
        ];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(false), // session_index miss for all
            || false,
            |e| e.name == "live-pane", // only live-pane's pid is alive
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(
            changes[0].new_status, None,
            "a live-pid mux pane is preserved"
        );
        assert_eq!(
            changes[1].new_status,
            Some(AgentStatus::Exited),
            "a dead-pid mux pane is reaped to Exited"
        );
        assert_eq!(
            changes[2].new_status,
            Some(AgentStatus::Orphaned),
            "a pid-less mux pane defers to store liveness (orphan), not immortal"
        );
        assert_eq!(out.orphans, vec!["pidless-pane".to_string()]);
    }

    #[test]
    fn reconcile_store_hit_does_not_resurrect_a_pid_dead_row() {
        // x-830c: a store that never evicts (opencode keeps its session rows
        // forever) answers Ok(true) long after the pane is gone. Recovery needs
        // the pid too, or every sweep would flip a dead orphan back to Live and
        // discovery would hand out a recipient nobody drains.
        let entries = vec![
            rentry("dead-orphan", AgentStatus::Orphaned, None),
            rentry("live-orphan", AgentStatus::Orphaned, None),
        ];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true), // session still in the store for both
            || false,
            |e| e.name == "live-orphan",
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(
            changes[0].new_status, None,
            "a store hit must not recover a row whose pid is dead"
        );
        assert_eq!(
            changes[1].new_status,
            Some(AgentStatus::Live),
            "a store hit on a live pid still recovers"
        );
        assert_eq!(out.recovered, vec!["live-orphan".to_string()]);
    }

    #[test]
    fn reconcile_pidless_orphan_still_recovers_on_store_hit() {
        // Guards the blast radius of the pid gate above: `pid_live` is true for a
        // row with no recorded pid, so exec rows keep their old behavior.
        let entries = vec![rentry("pidless", AgentStatus::Orphaned, None)];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(changes[0].new_status, Some(AgentStatus::Live));
        assert_eq!(out.recovered, vec!["pidless".to_string()]);
    }

    #[test]
    fn to_agent_entry_projects_the_opencode_session_id() {
        // Python persists opencode ids to harness_session_id and drops
        // `session_id` on write, so without this arm the probe would receive
        // None for every pane row and never run.
        let mut e = rentry("oc", AgentStatus::Live, None);
        e.legacy_provider = "opencode".into();
        e.harness = Some("opencode".into());
        e.harness_session_id = Some("ses_09679f284ffeJv7NdBAoLQLnLZ".into());
        e.session_id = None;
        assert_eq!(
            to_agent_entry(&e).session_id.as_deref(),
            Some("ses_09679f284ffeJv7NdBAoLQLnLZ")
        );
    }

    #[test]
    fn reconcile_inconclusive_preserves_status() {
        let entries = vec![rentry("flaky", AgentStatus::Live, None)];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Err(probe_err()),
            || false,
            |_| true,
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
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
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(false),
            || false,
            |_| true,
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
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
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
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
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(changes[0].new_status, None);
        assert!(out.updated.is_empty());
    }

    #[test]
    fn reconcile_does_not_reap_a_bg_thread_that_is_live_in_claudes_roster() {
        // x-beb7: a `claude --substrate bg` thread has claude's harness, no
        // footnote pid and no mux, so it matches `is_one_shot_ask` exactly like a
        // finished ask -- but it is a RUNNING process owned by claude's daemon.
        // Reaping it unprobed made `fno-agents wait --state done` return
        // "done (via exit)" within seconds for a worker whose transcript was
        // still growing, which reads to a waiting king as a dead teammate.
        let entries = vec![bg_claude_row("think-web-copy", "35570a01")];
        assert!(
            entries[0].is_one_shot_ask(),
            "a bg thread must still match the ask shape, or this test proves nothing"
        );

        // Present in the roster == running: leave the row alone.
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| true,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(
            changes[0].new_status, None,
            "a bg thread claude's daemon still lists must not be reaped to exited"
        );
        assert!(out.updated.is_empty());

        // Absent from the roster == genuinely gone: the ask reap still applies,
        // so this is a liveness check, not a blanket exemption for claude rows.
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(changes[0].new_status, Some(AgentStatus::Exited));
        assert_eq!(out.updated, vec!["think-web-copy".to_string()]);
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
        apply_reconcile_change(&mut to_exited, Some(AgentStatus::Exited), None, "T1");
        assert_eq!(to_exited.status, AgentStatus::Exited);
        assert_eq!(to_exited.pid, None, "exited row must drop its pid");
        assert_eq!(to_exited.pid_start_time, None);
        assert_eq!(
            to_exited.inside_leg, None,
            "exited row must clear the inside-leg authority (E3.3 / AC-X2-4)"
        );
        assert_eq!(to_exited.last_reconciled_at.as_deref(), Some("T1"));
        assert_eq!(
            to_exited.exited_at.as_deref(),
            Some("T1"),
            "the Exited transition stamps exited_at; CHECKED alone must not pose as one"
        );

        let mut to_orphaned = rentry("y", AgentStatus::Live, None);
        to_orphaned.pid = Some(4242);
        to_orphaned.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: "2026-06-27T00:00:00Z".into(),
            ttl_ms: None,
        });
        apply_reconcile_change(&mut to_orphaned, Some(AgentStatus::Orphaned), None, "T2");
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
        assert_eq!(
            to_orphaned.exited_at, None,
            "a non-exit transition writes no exit stamp"
        );

        // No status change: status held, but CHECKED still freshens (AC2-FR).
        let mut no_change = rentry("z", AgentStatus::Live, Some("OLD"));
        no_change.pid = Some(4242);
        apply_reconcile_change(&mut no_change, None, None, "T3");
        assert_eq!(no_change.status, AgentStatus::Live);
        assert_eq!(no_change.pid, Some(4242));
        assert_eq!(no_change.last_reconciled_at.as_deref(), Some("T3"));
        assert_eq!(
            no_change.exited_at, None,
            "a CHECKED-only probe must not write an exit stamp"
        );
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
        row.legacy_provider = "claude".into();
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
            |_| false,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive, // x-5d96 liveness: Alive flips nothing
            true,                   // roster readable: the flip needs a successful roster read
        );
        assert_eq!(out.deferred, 2, "two trailing entries should defer");
        assert_eq!(changes.len(), 1, "only one entry probed before budget");
    }

    /// A pane-hosted codex row: empty short_id (matches the real spawn shape),
    /// a mux ref, and whatever pid/session id the caller sets afterward.
    fn codex_pane_row(name: &str) -> RegistryEntry {
        let mut e = ask_row(name, None);
        e.harness = Some("codex".into());
        // x-7bcd: a real codex pane starts id-less (late-bind is what sets
        // harness_session_id), so undo ask_row's default and use a log_path
        // for the resolvable handle instead.
        e.harness_session_id = None;
        e.log_path = Some(format!("/tmp/{name}.log"));
        e.status = AgentStatus::Live;
        e.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 1,
        });
        e
    }

    #[test]
    fn late_bind_writes_harness_session_id_for_an_unbound_live_codex_pane() {
        // AC1 (x-9de7 task 2): a codex pane whose spawn-time bind window
        // expired carries no harness_session_id. One late-bind pass, given a
        // live pid, resolves and writes it.
        let home = tmp_home("late-bind-basic");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut e = codex_pane_row("pane-a");
            e.pid = Some(me);
            e.pid_start_time = Some(my_start);
            r.entries.push(e);
        })
        .unwrap();

        late_bind_codex_sessions(&home, &emitter, &|pid| {
            (pid == me).then(|| "sess-a".to_string())
        })
        .unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(
            reg.find("pane-a").unwrap().harness_session_id.as_deref(),
            Some("sess-a")
        );
        let events = read_events(&home);
        assert!(events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("agent_late_bind")));
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_surfaces_registry_write_failure() {
        let home = tmp_home("late-bind-write-failure");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut existing = ask_row("existing", None);
            existing.harness = Some("codex".into());
            existing.harness_session_id = Some("duplicate-session".into());
            r.entries.push(existing);

            let mut candidate = codex_pane_row("pane-a");
            candidate.pid = Some(me);
            candidate.pid_start_time = Some(my_start);
            r.entries.push(candidate);
        })
        .unwrap();

        let error =
            late_bind_codex_sessions(&home, &emitter, &|_| Some("duplicate-session".to_string()))
                .expect_err("duplicate session identity must fail the late-bind write");

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.find("pane-a").unwrap().harness_session_id.is_none());
        assert!(error.contains("late-bind registry write failed for pane-a"));
        let events = read_events(&home);
        assert!(events.iter().any(|e| {
            e.get("type").and_then(Value::as_str) == Some("agent_late_bind_failed")
                && e.get("data")
                    .and_then(|data| data.get("name"))
                    .and_then(Value::as_str)
                    == Some("pane-a")
        }));
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_gives_each_same_cwd_pane_its_own_session_id() {
        // The same-cwd repro (x-9de7 verification #5): two codex panes in one
        // cwd, both bound late, each keyed on its own pid -- this is the test
        // that would have caught a `(harness, cwd)` join.
        let home = tmp_home("late-bind-same-cwd");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut a = codex_pane_row("pane-a");
            a.cwd = "/repo".into();
            a.pid = Some(me);
            a.pid_start_time = Some(my_start);
            r.entries.push(a);

            let mut b = codex_pane_row("pane-b");
            b.cwd = "/repo".into();
            b.pid = Some(me);
            b.pid_start_time = Some(my_start);
            r.entries.push(b);
        })
        .unwrap();

        // A real probe is keyed on pid, so two DISTINCT pids would resolve to
        // two distinct sessions; here both rows share this test's own pid (no
        // second live process to fork), so the fake keys on name via a
        // once-per-call counter to prove per-row binding still lands
        // per-row rather than being skipped as "already bound" after the
        // first write.
        let calls = std::cell::RefCell::new(0);
        late_bind_codex_sessions(&home, &emitter, &|_pid| {
            let mut n = calls.borrow_mut();
            *n += 1;
            Some(format!("sess-{n}"))
        })
        .unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        let sid_a = reg.find("pane-a").unwrap().harness_session_id.clone();
        let sid_b = reg.find("pane-b").unwrap().harness_session_id.clone();
        assert!(sid_a.is_some() && sid_b.is_some());
        assert_ne!(sid_a, sid_b, "each pane must receive its own session id");
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_leaves_a_gone_pane_for_the_reaper() {
        // A pane-hosted row whose pane is gone: no session id is written and
        // the row is left for the reaper (x-9de7 task 2 AC3).
        let home = tmp_home("late-bind-gone-pane");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = codex_pane_row("pane-gone");
            e.pid = Some(0x7fff_fff0); // not a live process
            r.entries.push(e);
        })
        .unwrap();

        late_bind_codex_sessions(&home, &emitter, &|_| {
            panic!("the probe must not run against a pid that already fails pid_is_ours")
        })
        .unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.find("pane-gone").unwrap().harness_session_id.is_none());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_never_clobbers_an_already_bound_row() {
        let home = tmp_home("late-bind-already-bound");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut e = codex_pane_row("pane-bound");
            e.pid = Some(me);
            e.pid_start_time = Some(my_start);
            e.harness_session_id = Some("already-there".into());
            r.entries.push(e);
        })
        .unwrap();

        late_bind_codex_sessions(&home, &emitter, &|_| Some("already-there".to_string())).unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(
            reg.find("pane-bound")
                .unwrap()
                .harness_session_id
                .as_deref(),
            Some("already-there")
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_applies_a_dead_predecessor_succession() {
        let home = tmp_home("late-bind-succession");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut entry = codex_pane_row("pane-a");
            entry.pid = Some(me);
            entry.pid_start_time = Some(my_start);
            entry.harness_session_id = Some("session-a".into());
            entry.fno_id = Some("thread-a".into());
            r.entries.push(entry);
        })
        .unwrap();

        late_bind_codex_sessions_with_transition(
            &home,
            &emitter,
            &|_| Some("session-b".to_string()),
            &|session| (session == "session-a").then_some(false),
        )
        .unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        let entry = reg.find("pane-a").unwrap();
        assert_eq!(entry.harness_session_id.as_deref(), Some("session-b"));
        assert_eq!(entry.predecessor_session_ids, vec!["session-a"]);
        assert!(read_events(&home).iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("agent_late_bind")
                && event
                    .get("data")
                    .and_then(|data| data.get("transition"))
                    .and_then(Value::as_str)
                    == Some("succession")
        }));
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_does_not_apply_liveness_sampled_from_a_stale_predecessor() {
        let home = tmp_home("late-bind-stale-predecessor");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut entry = codex_pane_row("pane-a");
            entry.pid = Some(me);
            entry.pid_start_time = Some(my_start);
            entry.harness_session_id = Some("session-a".into());
            r.entries.push(entry);
        })
        .unwrap();

        let switched = std::cell::Cell::new(false);
        late_bind_codex_sessions_with_transition(
            &home,
            &emitter,
            &|_| {
                if !switched.replace(true) {
                    state::update_registry(&home.registry_json(), |r| {
                        r.find_mut("pane-a").unwrap().harness_session_id = Some("session-c".into());
                    })
                    .unwrap();
                }
                Some("session-b".to_string())
            },
            &|session| (session == "session-a").then_some(false),
        )
        .unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(
            reg.find("pane-a").unwrap().harness_session_id.as_deref(),
            Some("session-c")
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn late_bind_preserves_a_live_predecessor_and_creates_a_clean_branch_row() {
        let home = tmp_home("late-bind-branch");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return;
        };
        state::update_registry(&home.registry_json(), |r| {
            let mut entry = codex_pane_row("pane-a");
            entry.pid = Some(me);
            entry.pid_start_time = Some(my_start);
            entry.harness_session_id = Some("session-a".into());
            entry.fno_id = Some("thread-a".into());
            r.entries.push(entry);
        })
        .unwrap();

        late_bind_codex_sessions_with_transition(
            &home,
            &emitter,
            &|_| Some("session-b".to_string()),
            &|session| (session == "session-a").then_some(true),
        )
        .unwrap();

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.entries.len(), 2);
        let predecessor = reg.find("pane-a").unwrap();
        assert_eq!(predecessor.harness_session_id.as_deref(), Some("session-a"));
        let branch = reg
            .entries
            .iter()
            .find(|entry| entry.harness_session_id.as_deref() == Some("session-b"))
            .expect("branch session row");
        assert_eq!(branch.forked_from_session_id.as_deref(), Some("session-a"));
        assert_eq!(branch.fno_id.as_deref(), Some("session-b"));
        assert!(branch.short_id.is_empty());
        assert!(branch.mux.is_none());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn run_reconcile_sweep_empty_registry_is_noop() {
        // Boundaries (Architecture B): an empty registry sweeps cleanly -- no
        // entries, no changes -- the startup-path no-op case. Exercises the shared
        // sweep core (load -> sort -> write -> emit) directly.
        let home = tmp_home("sweep-empty");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let result = run_reconcile_sweep(&home, &emitter, &|_| false).expect("empty sweep ok");
        assert!(result.entries.is_empty());
        assert_eq!(result.outcome, ReconcileOutcome::default());
        std::fs::remove_dir_all(home.root()).ok();
    }

    // ------------------------------------------------------------------------
    // Registry-side keeper sweep (x-ac6b). A FAKE keeper answering Identify is
    // enough here: the real supervisor kill is the last group's journey and
    // this plan does not claim it.
    // ------------------------------------------------------------------------

    /// An agents home under a UNIQUE short base dir, so `mux/threads/` (the
    /// sweep's directory, derived from the agents root's parent) never collides
    /// between parallel tests the way a shared `/tmp/mux` would.
    fn keeper_sweep_home(tag: &str) -> AgentsHome {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(format!("/tmp/fnokswp{tag}{}_{n}", std::process::id()));
        // Pids recycle, so a prior run may own this path; a stale socket file
        // in it fails fixture binds with EADDRINUSE. Start from an empty base.
        let _ = std::fs::remove_dir_all(&base);
        let home = AgentsHome::at(base.join("agents"));
        home.ensure_root().unwrap();
        std::fs::create_dir_all(lane_b_keeper_dir(&home)).unwrap();
        home
    }

    /// A registry row shaped exactly like the lane-B spawn writes it (pi
    /// harness, interactive, socket-keyed, session id minted before launch).
    fn lane_b_thread_row(
        name: &str,
        session: &str,
        cwd: &str,
        child_pid: Option<u32>,
        sock: &Path,
    ) -> RegistryEntry {
        RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: Some("spawn".into()),
            name: name.into(),
            short_id: String::new(),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: Some("pi".into()),
            harness_session_id: Some(session.into()),
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            cwd: cwd.into(),
            project_root: String::new(),
            session_id: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            // The keeper's own pid: NOT the child pid, which rides
            // keeper_child_pid.
            pid: Some(4242),
            pid_start_time: None,
            keeper_child_pid: child_pid,
            messaging_socket_path: Some(sock.to_string_lossy().into_owned()),
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: Some("interactive".into()),
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-09-01T00:00:00Z".into(),
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            sandbox_posture: None,
            ..Default::default()
        }
    }

    /// A fake keeper behind `sock`, speaking the real frame protocol via the
    /// binary's own codec. `reply` is the IdentifyReply JSON it answers with.
    /// Exits after serving one Identify.
    fn spawn_fake_keeper(sock: &Path, reply: serde_json::Value) -> std::thread::JoinHandle<()> {
        use std::os::unix::net::UnixListener;
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(sock).unwrap();
        std::thread::Builder::new()
            .name("fake-keeper".into())
            .spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                serve_fake_identify(&mut stream, &reply);
            })
            .unwrap()
    }

    fn serve_fake_identify(stream: &mut std::os::unix::net::UnixStream, reply: &serde_json::Value) {
        use crate::pane_keeper::{decode, encode, Decode, Frame};
        use std::io::{Read, Write};
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            loop {
                match decode(&buf) {
                    Decode::NeedMore => break,
                    Decode::Violation(_) => return,
                    Decode::Frame(Frame::Identify, _) => {
                        let frame = encode(&Frame::IdentifyReply(reply.to_string().into_bytes()));
                        let _ = stream.write_all(&frame);
                        let _ = stream.flush();
                        // Hold the connection a beat so the probe's reply read
                        // is not EOF-raced.
                        std::thread::sleep(Duration::from_millis(100));
                        return;
                    }
                    Decode::Frame(_, used) => {
                        buf.drain(..used);
                    }
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// The AC4 wedged shape: accepts the connection and never answers. Parks
    /// until `stop` flips so the test can end the thread deterministically.
    fn spawn_silent_keeper(
        sock: &Path,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(sock).unwrap();
        std::thread::Builder::new()
            .name("silent-keeper".into())
            .spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                // Consume the Identify frame, answer nothing.
                let mut chunk = [0u8; 64];
                let _ = stream.read(&mut chunk);
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .unwrap()
    }

    /// A keeper that honors the Kill contract: on the Kill frame it unlinks
    /// its socket and stops serving, the way the real keeper exits after
    /// SIGKILLing its child (pane_keeper.rs). Parks until `stop` flips so the
    /// test can end the thread deterministically even on refusal paths.
    fn spawn_killable_keeper(
        sock: &Path,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use crate::pane_keeper::{decode, Decode, Frame};
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::Ordering::SeqCst;
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(sock).unwrap();
        let sock_path = sock.to_path_buf();
        std::thread::Builder::new()
            .name("killable-keeper".into())
            .spawn(move || {
                listener.set_nonblocking(true).unwrap();
                while !stop.load(SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let mut buf: Vec<u8> = Vec::new();
                            let mut chunk = [0u8; 4096];
                            loop {
                                match decode(&buf) {
                                    Decode::NeedMore => {}
                                    Decode::Violation(_) => return,
                                    Decode::Frame(frame, used) => {
                                        buf.drain(..used);
                                        if matches!(frame, Frame::Kill) {
                                            // The real keeper kills the child
                                            // here; there is no child to kill
                                            // behind the fake.
                                            let _ = std::fs::remove_file(&sock_path);
                                            stop.store(true, SeqCst);
                                            return;
                                        }
                                    }
                                }
                                match stream.read(&mut chunk) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                }
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => return,
                    }
                }
                let _ = std::fs::remove_file(&sock_path);
            })
            .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeper_thread_stop_confirms_the_kill_and_stamps_the_row_exited() {
        // PR 1332 review finding: a lane-B row's empty short_id fell into the
        // no-op arm, which reported a stop that stopped nothing. The keeper
        // arm must Kill over the row's own socket, CONFIRM the keeper went
        // away, and only then stamp the row terminal.
        let home = keeper_sweep_home("kpstop");
        let sock = lane_b_keeper_dir(&home).join("wk-stop.sock");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let keeper = spawn_killable_keeper(&sock, Arc::clone(&stop));
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(lane_b_thread_row(
                "wk-stop",
                "sess-1",
                "/repo",
                Some(555),
                &sock,
            ));
        })
        .unwrap();
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));

        let response = handle_stop(
            &ctx,
            &Request::new(1, "agent.stop", json!({"name": "wk-stop"})),
        )
        .await;
        let result = response.result().expect("stop errored");
        assert_eq!(result["stopped"], true, "{result:?}");
        assert_eq!(result["backend"], "keeper-thread", "{result:?}");

        keeper.join().unwrap();
        assert!(!sock.exists(), "the keeper unlinks its own socket on Kill");
        let registry = load_registry_offloaded(home.registry_json()).await.unwrap();
        let entry = registry.find("wk-stop").unwrap();
        assert_eq!(entry.status, AgentStatus::Exited);
        assert!(
            entry.exited_at.is_some(),
            "the terminal stamp carries a time"
        );
        assert!(read_events(&home).iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("agent_stopped")
                && event
                    .get("data")
                    .and_then(|data| data.get("backend"))
                    .and_then(Value::as_str)
                    == Some("keeper-thread")
        }));
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn keeper_thread_stop_refuses_when_the_keeper_never_confirms() {
        // A keeper that swallows the Kill frame leaves the row non-terminal:
        // reporting a stop over a live keeper is the zombie shape.
        let home = keeper_sweep_home("kprefu");
        let sock = lane_b_keeper_dir(&home).join("wk-stubborn.sock");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _keeper = spawn_silent_keeper(&sock, Arc::clone(&stop));
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(lane_b_thread_row(
                "wk-stubborn",
                "sess-2",
                "/repo",
                Some(555),
                &sock,
            ));
        })
        .unwrap();
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));

        let response = handle_stop(
            &ctx,
            &Request::new(2, "agent.stop", json!({"name": "wk-stubborn"})),
        )
        .await;
        assert!(
            response.result().is_none(),
            "an unconfirmed keeper must error, not report a stop"
        );
        let registry = load_registry_offloaded(home.registry_json()).await.unwrap();
        assert_ne!(
            registry.find("wk-stubborn").map(|entry| entry.status),
            Some(AgentStatus::Exited),
            "a refused stop must not stamp the row terminal"
        );
        assert!(sock.exists(), "a refused stop never unlinks the socket");
        assert!(read_events(&home).iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("agent_stop_refused")
        }));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_worker_confirmed_routes_a_keeper_row_to_its_own_socket() {
        // The forced-rm orphan: a lane-B row's empty short_id derived
        // worker_sock(""), and the probe over that absent socket confirmed a
        // stop over a socket the keeper does not own. The delegation must
        // Kill the row's OWN socket.
        let home = keeper_sweep_home("kprm");
        let sock = lane_b_keeper_dir(&home).join("wk-rm.sock");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let keeper = spawn_killable_keeper(&sock, Arc::clone(&stop));
        let entry = lane_b_thread_row("wk-rm", "sess-3", "/repo", Some(555), &sock);
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));

        let confirmed = stop_worker_confirmed(&ctx, &entry).await;
        keeper.join().unwrap();
        assert!(confirmed, "a Kill-honoring keeper confirms down");
        assert!(!sock.exists());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn keeper_sweep_apply_discards_a_verdict_when_the_row_changed_identity() {
        // The P1 race: the sweep snapshots the registry, probes up to the
        // budget while the daemon already serves, and an operator removes the
        // row and re-spawns under the SAME name before the write. A name-only
        // apply would stamp the old keeper's verdict onto the healthy
        // replacement.
        let mut registry = state::Registry::default();
        registry.entries.push(lane_b_thread_row(
            "wk-raced",
            "sess-new",
            "/tmp",
            None,
            Path::new("/tmp/mux/threads/wk-raced.sock"),
        ));
        let changes = vec![KeeperSweepChange {
            name: "wk-raced".into(),
            status: Some(AgentStatus::Exited),
            child_pid: None,
            bound_socket: Some("/tmp/mux/threads/wk-raced.sock".into()),
            bound_session: Some("sess-old".into()),
        }];
        let superseded =
            apply_keeper_sweep_changes(&mut registry, &changes, "2026-09-01T00:00:00Z");
        assert_eq!(
            superseded,
            vec!["wk-raced"],
            "the raced row is named superseded"
        );
        assert_eq!(
            registry.entries[0].status,
            AgentStatus::Live,
            "the replacement row keeps its own status"
        );
        // The same change against the row it was probed from still applies.
        let mut matching = state::Registry::default();
        matching.entries.push(lane_b_thread_row(
            "wk-raced",
            "sess-old",
            "/tmp",
            None,
            Path::new("/tmp/mux/threads/wk-raced.sock"),
        ));
        let superseded =
            apply_keeper_sweep_changes(&mut matching, &changes, "2026-09-01T00:00:00Z");
        assert!(superseded.is_empty(), "identity-held change applies");
        assert_eq!(matching.entries[0].status, AgentStatus::Exited);
    }

    #[test]
    fn keeper_registry_sweep_flags_a_moved_cwd_even_when_the_child_pid_matches() {
        // The P2 chain bug: an else-if identity ladder skips the cwd leg
        // whenever both child pids are present and equal, re-binding a keeper
        // that answers from a different directory.
        let home = keeper_sweep_home("cwd");
        let threads = lane_b_keeper_dir(&home);
        let recorded_cwd = home.root().parent().unwrap().to_string_lossy().into_owned();
        let elsewhere = home.root().to_string_lossy().into_owned();
        let sock = threads.join("wk-moved.sock");
        let keeper = spawn_fake_keeper(
            &sock,
            json!({
                "v": 1, "keeper_pid": 4242, "child_pid": 111,
                "session_id": "sess-moved", "cwd": elsewhere, "argv": [],
            }),
        );
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(lane_b_thread_row(
                "wk-moved",
                "sess-moved",
                &recorded_cwd,
                Some(111),
                &sock,
            ));
        })
        .unwrap();

        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let report = keeper_registry_sweep(&home, &emitter).expect("sweep ok");
        assert!(report.rebound.is_empty(), "a moved cwd must not re-bind");
        assert_eq!(report.dead.len(), 1, "the moved cwd is named dead");
        assert!(
            report.dead[0].1.contains("cwd"),
            "the reason names the cwd mismatch: {}",
            report.dead[0].1
        );
        let row = state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .remove(0);
        assert_eq!(row.status, AgentStatus::Exited);
        keeper.join().unwrap();
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn keeper_registry_sweep_names_every_outcome_and_bounds_the_wedged_probe() {
        let home = keeper_sweep_home("all");
        let threads = lane_b_keeper_dir(&home);
        let cwd = home.root().parent().unwrap().to_string_lossy().into_owned();
        let sock = |name: &str| threads.join(format!("{name}.sock"));

        // Live: the keeper answers the row's own identity.
        let live = spawn_fake_keeper(
            &sock("wk-live"),
            json!({
                "v": 1, "keeper_pid": 4242, "child_pid": 111,
                "session_id": "sess-live", "cwd": cwd, "argv": ["pi", "--session-id", "sess-live"],
            }),
        );
        // Clone: a keeper answering a DIFFERENT session id under the row's
        // socket - the respawn-wearing-the-name failure (AC3-ERR).
        let clone = spawn_fake_keeper(
            &sock("wk-clone"),
            json!({"v": 1, "keeper_pid": 5, "child_pid": 6, "session_id": "sess-other", "cwd": cwd}),
        );
        // Respawn: same session id, DIFFERENT child pid. Passes any liveness
        // check and must still fail this one - that is the point.
        let respawn = spawn_fake_keeper(
            &sock("wk-respawn"),
            json!({"v": 1, "keeper_pid": 7, "child_pid": 999, "session_id": "sess-respawn", "cwd": cwd}),
        );
        // Wedged: accepts, never answers (AC4-ERR).
        let wedge_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wedge = spawn_silent_keeper(&sock("wk-wedge"), Arc::clone(&wedge_stop));
        // Dead: a socket file with no listener behind it.
        std::fs::write(sock("wk-dead"), b"").unwrap();

        let rows = [
            lane_b_thread_row("wk-live", "sess-live", &cwd, Some(111), &sock("wk-live")),
            lane_b_thread_row("wk-clone", "sess-clone", &cwd, Some(222), &sock("wk-clone")),
            lane_b_thread_row(
                "wk-respawn",
                "sess-respawn",
                &cwd,
                Some(111),
                &sock("wk-respawn"),
            ),
            lane_b_thread_row("wk-wedge", "sess-wedge", &cwd, Some(333), &sock("wk-wedge")),
            lane_b_thread_row("wk-dead", "sess-dead", &cwd, Some(444), &sock("wk-dead")),
        ];
        state::update_registry(&home.registry_json(), |r| {
            r.entries.extend(rows.iter().cloned());
        })
        .unwrap();

        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let start = Instant::now();
        let report = keeper_registry_sweep(&home, &emitter).expect("sweep ok");
        let elapsed = start.elapsed();

        // All five sockets examined; exactly one re-bound.
        assert_eq!(report.sockets, 5);
        assert_eq!(
            report.rebound,
            vec!["wk-live"],
            "only the live keeper re-binds"
        );
        let dead_names: Vec<&str> = report.dead.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            dead_names,
            vec!["wk-clone", "wk-dead", "wk-respawn"],
            "every dead row is named, sorted by socket"
        );
        let clone_reason = &report.dead[0].1;
        assert!(
            clone_reason.contains("session id") && clone_reason.contains("sess-other"),
            "the clone's reason names the session mismatch: {clone_reason}"
        );
        assert!(
            report.dead[2].1.contains("child pid changed"),
            "the respawn's reason names the pid change: {}",
            report.dead[2].1
        );
        assert_eq!(report.wedged[0].0, "wk-wedge", "the wedged row is named");
        assert!(
            report.wedged[0].1.contains("did not answer"),
            "the wedged reason names the silence: {}",
            report.wedged[0].1
        );
        // The stale socket is unlinked; live listeners are never unlinked.
        assert_eq!(
            report.unlinked,
            vec![sock("wk-dead").to_string_lossy().into_owned()]
        );
        assert!(sock("wk-clone").exists(), "a live keeper's socket stays");
        assert!(sock("wk-wedge").exists(), "a wedged keeper's socket stays");
        // Bounded: one wedged probe costs one reply timeout, not a hang.
        assert!(
            elapsed < Duration::from_secs(5),
            "the sweep completed inside its budget, took {elapsed:?}"
        );

        let registry = state::load_registry(&home.registry_json()).unwrap();
        let row = |name: &str| {
            registry
                .entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} row"))
                .clone()
        };
        assert_eq!(row("wk-live").status, AgentStatus::Live);
        assert_eq!(row("wk-live").keeper_child_pid, Some(111));
        assert_eq!(row("wk-clone").status, AgentStatus::Exited);
        assert_eq!(row("wk-respawn").status, AgentStatus::Exited);
        // The recorded child pid survives the dead verdict as forensics.
        assert_eq!(row("wk-respawn").keeper_child_pid, Some(111));
        assert_eq!(row("wk-dead").status, AgentStatus::Exited);
        assert_eq!(
            row("wk-dead").pid,
            None,
            "Exited clears the stale pid (Locked 7)"
        );
        // Silence never proves death: the wedged row is untouched.
        assert_eq!(row("wk-wedge").status, AgentStatus::Live);

        // End the fake keepers before teardown.
        wedge_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for handle in [live, clone, respawn, wedge] {
            handle.join().unwrap();
        }
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn keeper_reattach_identity_asserts_cwd_session_and_child_pid_across_the_restart() {
        let home = keeper_sweep_home("id");
        let threads = lane_b_keeper_dir(&home);
        let cwd = home.root().parent().unwrap().to_string_lossy().into_owned();
        // A child pid that is PROVABLY alive: this test process. The keeper
        // answers it, the row records it, and the assertion that the exact
        // pid survived the restart is a real signal(0), not a string compare.
        let child_pid = std::process::id();
        let sock = threads.join("wk-pi.sock");
        let keeper = spawn_fake_keeper(
            &sock,
            json!({
                "v": 1, "keeper_pid": 4242, "child_pid": child_pid,
                "session_id": "sess-pi", "cwd": cwd, "argv": ["pi", "--session-id", "sess-pi"],
            }),
        );
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(lane_b_thread_row(
                "wk-pi",
                "sess-pi",
                &cwd,
                Some(child_pid),
                &sock,
            ));
        })
        .unwrap();

        // The restart: the daemon died and came back, and its startup sweep is
        // the only thing that walks the socket back to the row.
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let report = keeper_registry_sweep(&home, &emitter).expect("sweep ok");
        assert_eq!(report.rebound, vec!["wk-pi"]);
        assert!(report.dead.is_empty() && report.wedged.is_empty());

        let row = state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.name == "wk-pi")
            .unwrap();
        // Byte-equal identity across the restart, and nothing new minted.
        assert_eq!(row.status, AgentStatus::Live, "the row is live again");
        assert_eq!(row.harness_session_id.as_deref(), Some("sess-pi"));
        assert_eq!(row.cwd, cwd, "cwd is unchanged");
        assert_eq!(
            row.keeper_child_pid,
            Some(child_pid),
            "the child pid is unchanged"
        );
        assert_eq!(row.pid, Some(4242), "the keeper pid field is untouched");
        // The exact pid is still alive - same child, not a respawn wearing it.
        // SAFETY: signal 0 against this process's own pid is a pure liveness
        // probe; it delivers no signal.
        assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

        keeper.join().unwrap();
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
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
        // v25: the vendor route stays UNKNOWN on this lane (it may be routed;
        // the row's `provider` is None for the same reason), but the account
        // record mirrors the launch read rather than sitting at None by
        // omission - with no ambient config dir this env resolves "default".
        assert_eq!(row.route_provider_id, None);
        assert_eq!(row.model_name, None);
        assert_eq!(
            row.account_record_id.as_deref(),
            crate::state::launch_account_from_env().as_deref()
        );
    }

    /// A stub family-1 probe answer for the tests that only pin the state.
    /// `observed_model` null here stands for "this probe did not answer it",
    /// which the row renders as `no-transcript` rather than inventing a model.
    ///
    /// `reachability: None` deliberately exercises the COMPATIBILITY FALLBACK in
    /// `rendered_status_from_truth` (a `fno` too old to emit the verdict), which
    /// is what keeps these pre-existing state-mapping assertions meaningful.
    /// `probe_reachable` below covers the current wire.
    fn probe(state: &str) -> Option<crate::truth_probe::TruthProbe> {
        Some(crate::truth_probe::TruthProbe {
            state: state.into(),
            reachability: None,
            basis: None,
            last_activity_age_s: None,
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
            harness_title: None,
        })
    }

    /// The dormant gate's batch seam answering for nobody: no row's tail is
    /// readable. Every caller below has no live rows to probe, so this is the
    /// same "the probe said nothing" input the per-handle `None` used to be.
    // -- The shared liveness ladder (x-5d96) ---------------------------------
    use crate::client_verbs::row_liveness;

    /// A per-row prober mirroring [`fn@live_liveness_prober`] for the staged
    /// answer: same positive-death fold, index built per call.
    fn ladder_claude_row(name: &str, short_id: &str) -> RegistryEntry {
        let mut e = bg_claude_row(name, short_id);
        e.status = AgentStatus::Live;
        e
    }

    fn heartbeat(state: state::InsideLegState, received_at: &str) -> state::InsideLegReport {
        state::InsideLegReport {
            state,
            seq: 3,
            reason: None,
            received_at: received_at.into(),
            ttl_ms: None,
        }
    }

    /// Write a claude bg session file whose messaging socket is `sock`, so the
    /// in-process socket rung can connect to a REAL listener.
    fn write_bg_session(root: &Path, short_id: &str, sock: &Path) {
        let sessions = root.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let body = format!(
            "{{\"jobId\":\"{short_id}\",\"kind\":\"bg\",\"messagingSocketPath\":\"{}\",\"sessionId\":\"sess-{short_id}\",\"cwd\":\"/tmp\"}}",
            sock.to_str().unwrap()
        );
        std::fs::write(sessions.join("111.json"), body).unwrap();
    }

    /// A bindable unix socket path short enough for SUN_LEN (104): the
    /// sandbox tmp roots run long, and a socket path that long refuses to
    /// bind. Pid-suffixed so parallel test runs never share one.
    fn short_sock(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fno5d96-{}-{tag}.sock", std::process::id()))
    }

    #[test]
    fn the_ladder_answers_alive_for_a_known_live_socket() {
        // POSITIVE CONTROL (x-5d96): an absence-only suite proves nothing - a
        // probe that answers Unknown for every input passes every negative
        // test. Assert Alive on a session known to be live first.
        let home = tmp_home("ladder-positive-control");
        let sock = short_sock("alive");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        write_bg_session(home.root(), "alive01", &sock);
        let e = ladder_claude_row("positive", "alive01");
        let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
        assert_eq!(answer, RowLiveness::Alive);
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn king_g4_shape_is_alive_from_the_socket_rung_and_never_dead() {
        // The measured specimen: status live, exited_at ten days old, a
        // heartbeat sixteen hours stale reading done, process up 4h41m. The
        // socket rung answers; the quiet heartbeat never downgrades the
        // answer to anything, and `Dead` is not producible from absence.
        let home = tmp_home("ladder-g4");
        let sock = short_sock("g4");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        write_bg_session(home.root(), "g4face", &sock);
        let mut e = ladder_claude_row("king-footnote-g4", "g4face");
        e.exited_at = Some("2026-08-21T00:42:40Z".into());
        e.inside_leg = Some(heartbeat(
            state::InsideLegState::Done,
            "2026-08-31T07:51:14Z",
        ));
        let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
        assert_eq!(answer, RowLiveness::Alive);
        assert_ne!(answer, RowLiveness::Dead);
        drop(listener);
        let _ = std::fs::remove_file(&sock);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_heartbeat_advancing_past_exited_at_is_alive_x_d3ad() {
        // The adopted specimen: exited_at two seconds after created_at while
        // the heartbeat kept advancing past it. No socket answers; the
        // heartbeat rung does.
        let home = tmp_home("ladder-d3ad");
        let mut e = ladder_claude_row("resurrected", "d3adrow");
        e.created_at = "2026-08-01T00:00:00Z".into();
        e.exited_at = Some("2026-08-01T00:00:02Z".into());
        e.inside_leg = Some(heartbeat(
            state::InsideLegState::Working,
            "2026-08-01T00:00:30Z",
        ));
        let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
        assert_eq!(answer, RowLiveness::Alive);
        assert_ne!(answer, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_codex_row_with_no_socket_and_no_transcript_is_unknown_never_dead() {
        // Codex is invisible to the claude surfaces BY CONSTRUCTION. Silence
        // on every rung is Unknown - never a death verdict.
        let home = tmp_home("ladder-codex");
        let e = rentry("codex-thread", AgentStatus::Live, None);
        let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
        assert_eq!(answer, RowLiveness::Unknown);
        assert_ne!(answer, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    // -- Rung 4: codex rollout freshness (x-798a) ----------------------------

    fn codex_truth_none(_uuid: &str) -> Option<String> {
        None
    }

    /// A codex registry row in the measured x-798a shape: live-ish status,
    /// no claude surfaces, one harness session id, no exit stamp (no pid to
    /// confirm dead, no reconcile to terminal - the row never gets one).
    fn ladder_codex_row(name: &str, session_id: &str) -> RegistryEntry {
        let mut e = rentry(name, AgentStatus::Live, None);
        e.harness = Some("codex".into());
        e.harness_session_id = Some(session_id.into());
        e.claude_session_uuid = None;
        e
    }

    /// The real codex store shape: nested date dirs, the session id embedded
    /// in a `rollout-*.jsonl` filename - what `HarnessStoreIndex` resolves.
    fn write_rollout(root: &Path, session_id: &str) -> std::path::PathBuf {
        let dir = root.join("2026").join("09").join("01");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("rollout-2026-09-01T10-00-00-{session_id}.jsonl"));
        std::fs::write(&p, "{\"type\":\"session_meta\"}\n").unwrap();
        p
    }

    #[test]
    fn an_advancing_codex_rollout_answers_alive_and_the_sweep_names_it_live() {
        // AC1-HP (x-798a): a rollout jsonl written within the window proves
        // the worker is advancing. The heartbeat rung cannot fire here (no
        // exited_at to advance past) and the claude rungs cannot see the row;
        // without this rung the row is kept but never probeable.
        let home = tmp_home("ladder-codex-alive");
        let codex = tempfile::tempdir().unwrap();
        write_rollout(codex.path(), "cdx-alive");
        let e = ladder_codex_row("codex-live", "cdx-alive");
        let answer = crate::client_verbs::row_liveness_with_codex_root(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            codex.path(),
            codex_truth_none,
        );
        assert_eq!(answer, RowLiveness::Alive);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_codex_row_with_no_readable_rollout_stays_unknown_and_kept() {
        // AC1-EDGE + AC2-EDGE: a missing rollout and an unreadable store both
        // read Unknown (an absent store has two explanations and only one of
        // them is a dead worker), and the policy names the row not-terminal.
        // This IS the measured x-798a baseline: named, kept, never reaped.
        let home = tmp_home("ladder-codex-absent");
        let codex = tempfile::tempdir().unwrap();
        let e = ladder_codex_row("codex-gone", "cdx-gone");
        let answer = crate::client_verbs::row_liveness_with_codex_root(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            codex.path(),
            codex_truth_none,
        );
        assert_eq!(answer, RowLiveness::Unknown);
        let answer = crate::client_verbs::row_liveness_with_codex_root(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            &codex.path().join("does-not-exist"),
            codex_truth_none,
        );
        assert_eq!(answer, RowLiveness::Unknown);
        assert_ne!(answer, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_claude_row_is_never_judged_by_the_codex_store() {
        // AC3 (harness keying): a claude row whose session id has a FRESH
        // rollout in the codex store must still answer Unknown. The rung
        // fires only for codex rows; keyed on session id alone, one fresh
        // codex store would vouch for every claude row on the machine.
        let home = tmp_home("ladder-codex-keying");
        let codex = tempfile::tempdir().unwrap();
        write_rollout(codex.path(), "shared-sess");
        let mut e = ladder_claude_row("claude-row", "");
        e.harness = Some("claude".into());
        e.harness_session_id = Some("shared-sess".into());
        e.claude_session_uuid = None;
        let answer = crate::client_verbs::row_liveness_with_codex_root(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            codex.path(),
            codex_truth_none,
        );
        assert_eq!(answer, RowLiveness::Unknown);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_quiet_codex_rollout_proves_nothing() {
        // AC4-EDGE: an mtime older than the window falls through to Unknown.
        // Quiet is idle-or-dead and the ladder cannot say which - so it says
        // neither, and the row is kept.
        let home = tmp_home("ladder-codex-stale");
        let codex = tempfile::tempdir().unwrap();
        let p = write_rollout(codex.path(), "cdx-stale");
        let mtime = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs((now_epoch_secs() - 2 * 3600) as u64);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let e = ladder_codex_row("codex-stale", "cdx-stale");
        let answer = crate::client_verbs::row_liveness_with_codex_root(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            codex.path(),
            codex_truth_none,
        );
        assert_eq!(answer, RowLiveness::Unknown);
        assert_ne!(answer, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_stamped_codex_row_is_never_resurrected_by_a_fresh_rollout() {
        // The stop verb writes a POSITIVE exit stamp; the rollout it leaves
        // behind stays fresh-written for the whole window. Rung 4 must go
        // silent on a stamped row, or the sweep would clear the stamp and
        // resurrect a deliberately stopped row on freshness alone (the codex
        // review finding). The heartbeat rung draws the same line - only
        // ADVANCEMENT past the stamp answers, never a quiet file.
        let home = tmp_home("ladder-codex-stamped");
        let codex = tempfile::tempdir().unwrap();
        write_rollout(codex.path(), "cdx-stopped");
        let mut e = ladder_codex_row("codex-stopped", "cdx-stopped");
        e.exited_at = Some("2026-09-01T10:30:00Z".into());
        let answer = crate::client_verbs::row_liveness_with_codex_root(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            codex.path(),
            codex_truth_none,
        );
        assert_eq!(answer, RowLiveness::Unknown);
        assert_ne!(answer, RowLiveness::Alive);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn the_heartbeat_rung_is_the_codex_arm() {
        // The harness-agnostic rung: 14 of 26 rows are codex, and absence
        // from `fno agents top` is never a rung - but an advancing
        // inside_leg report is a positive marker on any harness.
        let home = tmp_home("ladder-codex-arm");
        let mut e = rentry("codex-thread", AgentStatus::Live, None);
        e.exited_at = Some("2026-08-01T00:00:02Z".into());
        e.inside_leg = Some(heartbeat(
            state::InsideLegState::Working,
            "2026-08-01T00:00:30Z",
        ));
        let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
        assert_eq!(answer, RowLiveness::Alive);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn an_unreadable_transcript_is_unknown_never_dead() {
        // Measured 2026-08-31: a shell-loop probe lost its expansion, globbed
        // nothing, and returned no-transcript for four rows whose files
        // existed. A failed read is silence, not death - daemon.rs's
        // transcript_fresh_probe states the same rule. The read here returns
        // nothing because it FAILED, and the answer stays Unknown.
        let home = tmp_home("ladder-unreadable");
        let mut e = ladder_claude_row("silent", "quiet01");
        e.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
        let answer = crate::client_verbs::row_liveness_with(
            &e,
            &crate::claude_ask::ClaudeHome::at(home.root()),
            |_| None,
        );
        assert_eq!(answer, RowLiveness::Unknown);
        assert_ne!(answer, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn an_expired_ttl_heartbeat_is_not_a_marker() {
        // The report's own trust contract ages a TTL'd stamp out
        // (InsideLegReport::is_live_at), so an expired beat answers nothing:
        // Unknown, never Dead, and never a forever-Alive pinning the row
        // against every later guard.
        let home = tmp_home("ladder-ttl");
        let mut e = ladder_claude_row("ttl", "ttls0001");
        e.exited_at = Some("2026-06-01T00:00:00Z".into());
        e.inside_leg = Some(heartbeat(
            state::InsideLegState::Working,
            "2026-06-01T00:00:30Z",
        ));
        e.inside_leg.as_mut().unwrap().ttl_ms = Some(60_000); // expired long ago
        let answer = row_liveness(&e, &crate::claude_ask::ClaudeHome::at(home.root()));
        assert_eq!(answer, RowLiveness::Unknown);
        assert_ne!(answer, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn the_truth_rung_separates_working_from_done() {
        // Four idle-done rows measured 2026-08-31 sat idle 40 to 47 minutes
        // with status live: the transcript tail separates done from working
        // and the status field cannot. `working` is a positive marker;
        // `done` is a TURN state and answers nothing - Unknown, not Dead.
        let home = tmp_home("ladder-truth-separation");
        let mut working = ladder_claude_row("working-row", "work001");
        working.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
        let mut done = ladder_claude_row("done-row", "done001");
        done.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae5".into());
        let ch = crate::claude_ask::ClaudeHome::at(home.root());
        let is_working = crate::client_verbs::row_liveness_with(&working, &ch, |u| {
            (u.ends_with('4')).then(|| "working".to_string())
        });
        assert_eq!(is_working, RowLiveness::Alive);
        let is_done = crate::client_verbs::row_liveness_with(&done, &ch, |u| {
            (u.ends_with('5')).then(|| "done".to_string())
        });
        assert_eq!(is_done, RowLiveness::Unknown);
        assert_ne!(is_done, RowLiveness::Dead);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn the_ladder_never_returns_dead() {
        // Only a positive death proof may answer Dead, and absence never is
        // one. Sweep every rung combination the ladder can see; no input
        // yields Dead.
        let home = tmp_home("ladder-never-dead");
        let ch = crate::claude_ask::ClaudeHome::at(home.root());
        for truth in [
            None,
            Some("done"),
            Some("stalled"),
            Some("unreachable"),
            Some("working"),
            Some("watching"),
            Some("your-move"),
        ] {
            for exited in [None, Some("2026-08-01T00:00:02Z")] {
                for beat in [
                    None,
                    Some(("2026-07-31T00:00:00Z", false)), // before exited: silence
                    Some(("2026-08-01T00:00:30Z", true)),  // after exited: Alive
                ] {
                    let mut e = ladder_claude_row("row", "quiet02");
                    e.exited_at = exited.map(String::from);
                    e.inside_leg =
                        beat.map(|(at, _)| heartbeat(state::InsideLegState::Working, at));
                    e.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
                    let answer = crate::client_verbs::row_liveness_with(&e, &ch, |_| {
                        truth.map(String::from)
                    });
                    assert_ne!(
                        answer,
                        RowLiveness::Dead,
                        "ladder answered Dead for truth={truth:?} exited={exited:?} beat={beat:?}"
                    );
                }
            }
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn an_unreadable_roster_never_flips_the_zombie_arm() {
        // codex P1 (PR 1329): an unreadable roster is unknown liveness. The
        // fail-closed bg_live reading must not combine with a transient
        // ladder failure to orphan every live bg worker - the flip needs a
        // roster read that SUCCEEDED.
        let entries = vec![bg_claude_row("maybe", "mayb0001")];
        let (changes, _) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| true, // bg_live fail-closed true on the unreadable roster
            |_| false,
            |_| false,
            |_| RowLiveness::Unknown,
            false, // the roster read itself FAILED
        );
        assert_eq!(changes[0].new_status, None);
    }

    #[test]
    fn the_orphan_transition_starts_a_fresh_grace_clock() {
        // codex P2 (PR 1329): the flip just re-decided liveness from current
        // evidence, so a carried exited_at is a stamp from a falsified
        // reading. apply clears it, so gc stamps fresh at the first real
        // dead-observation instead of aging the row on a clock that predates
        // the re-decision and skips grace.
        let mut e = bg_claude_row("zombie", "zomb0001");
        e.exited_at = Some("2026-08-21T00:42:40Z".into());
        apply_reconcile_change(
            &mut e,
            Some(AgentStatus::Orphaned),
            None,
            "2026-09-01T00:00:00Z",
        );
        assert_eq!(e.status, AgentStatus::Orphaned);
        assert!(e.exited_at.is_none());
    }

    #[test]
    fn reconcile_ends_the_status_constant_for_a_roster_stale_silent_row() {
        // The roster hit used to hold a claude row `live` forever: presence
        // in a possibly-stale roster proves nothing about RUNNING. A silent
        // ladder (no socket, no advancing heartbeat, no working truth state)
        // means no positive running-marker, so the ask-bucket row goes
        // Orphaned - the REVERSIBLE transition, not Exited - and gc takes it
        // from the terminal set.
        let entries = vec![bg_claude_row("zombie", "zomb0001")];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| true, // roster: entry present
            |_| false,
            |_| false,
            |_| RowLiveness::Unknown,
            true, // roster readable
        );
        assert_eq!(changes[0].new_status, Some(AgentStatus::Orphaned));
        assert_eq!(out.orphans, vec!["zombie".to_string()]);
    }

    #[test]
    fn an_alive_ladder_blocks_the_reconcile_zombie_flip() {
        // The x-d3ad resurrected session: roster present, heartbeat
        // advancing. The positive marker answers Alive and the flip must not
        // fire.
        let entries = vec![bg_claude_row("resurrected", "rise0001")];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| true,
            |_| false,
            |_| false,
            |_| RowLiveness::Alive,
            true, // roster readable
        );
        assert_eq!(changes[0].new_status, None);
        assert!(out.orphans.is_empty());
    }

    #[test]
    fn a_spawning_row_is_never_flipped_by_the_zombie_arm() {
        // A row still coming up has had no chance to produce ANY marker, so
        // its ladder silence is meaningless: Spawning is excluded from the
        // zombie arm (the sweep's never-reap-something-still-coming-up rule).
        let mut e = bg_claude_row("spawning", "spawn001");
        e.status = AgentStatus::Spawning;
        let entries = vec![e];
        let (changes, _) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| true,
            |_| false,
            |_| false,
            |_| RowLiveness::Unknown,
            true, // roster readable
        );
        assert_eq!(changes[0].new_status, None);
    }

    #[test]
    fn a_roster_miss_still_flips_the_ask_bucket_to_exited_as_before() {
        // The x-5d96 arm is ADDITIVE: the existing positive roster-miss -> Exited
        // contract is untouched.
        let entries = vec![bg_claude_row("finished", "fini0001")];
        let (changes, _) = plan_reconcile(
            &entries,
            |_| Ok(true),
            || false,
            |_| true,
            |_| false, // roster: positively gone
            |_| false,
            |_| false,
            |_| RowLiveness::Alive,
            true, // roster readable
        );
        assert_eq!(changes[0].new_status, Some(AgentStatus::Exited));
    }

    /// Adapt a per-handle answer into the BATCH seam `handle_list_with_truth`
    /// takes, for the rendering tests below - they assert what a row renders,
    /// never how many processes paid for it.
    ///
    /// Deliberately NOT used by
    /// `list_pays_exactly_one_batch_call_for_the_whole_page`: an adapter that
    /// fans a batch back out per handle would render identically whether the
    /// handler called it once or once per row, so the call SHAPE needs its own
    /// test against the raw seam.
    fn per_handle(
        f: impl Fn(&str) -> Option<crate::truth_probe::TruthProbe>,
    ) -> impl Fn(&[String]) -> std::collections::HashMap<String, crate::truth_probe::TruthProbe>
    {
        move |handles: &[String]| {
            handles
                .iter()
                .filter_map(|h| Some((h.clone(), f(h)?)))
                .collect()
        }
    }

    /// A probe carrying the shared verdict, as a current `fno` emits it.
    fn probe_with_verdict(
        state: &str,
        reachability: &str,
    ) -> Option<crate::truth_probe::TruthProbe> {
        Some(crate::truth_probe::TruthProbe {
            state: state.into(),
            reachability: Some(reachability.into()),
            basis: Some("transcript".into()),
            last_activity_age_s: Some(12.0),
            last_event_at: Some("2026-08-15T17:00:00+00:00".into()),
            last_message: Some(
                "Still growing (101 lines, 26 percent through the pytest run)".into(),
            ),
            observed_model: Value::Null,
            harness_title: None,
        })
    }

    fn probe_with_age(
        state: &str,
        reachability: &str,
        age_s: Option<f64>,
    ) -> Option<crate::truth_probe::TruthProbe> {
        let mut probe = probe_with_verdict(state, reachability).unwrap();
        probe.last_activity_age_s = age_s;
        Some(probe)
    }

    /// The verdict OUTRANKS the transcript state, which is the whole point of
    /// putting it on the wire: a session whose process died forty minutes ago
    /// still reads `working` from its transcript, and mapping that state is how
    /// `list` reported a dead worker live. Same input state, opposite render.
    #[test]
    fn the_reachability_verdict_outranks_the_transcript_state() {
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("working", "unreachable").as_ref()),
            "orphaned",
            "a falsified row must not render writing merely because its transcript is recent"
        );
        // A verdict that did not resolve falls through to the ACTIVITY read:
        // a 12s-old transcript is writing whatever the state word says.
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("stalled", "unknown").as_ref()),
            "writing"
        );
        assert_eq!(
            rendered_status_from_truth(probe("stalled").as_ref()),
            "unknown",
            "a probe that answered nothing reads unknown, never orphaned (x-c672)"
        );
    }

    #[test]
    fn no_probe_at_all_reads_unknown_even_for_a_live_row() {
        // A live pid is a fact about the PROCESS, not about served activity,
        // so it is not an input to the STATUS word (x-c672): the Python list
        // lane has no pid census, and an unanswered activity age must read
        // the same word on both lanes.
        assert_eq!(
            rendered_status_from_truth(None),
            "unknown",
            "no probe at all reads unknown, never quiet"
        );
    }

    /// A verdict-carrying probe with an explicit `observed_model`, for the
    /// progress-axis tests below (`probe_with_verdict` above always carries
    /// `Value::Null`, which is `no-transcript` and can never refuse).
    fn probe_observed(
        state: &str,
        reachability: &str,
        observed_model: Value,
    ) -> Option<crate::truth_probe::TruthProbe> {
        Some(crate::truth_probe::TruthProbe {
            state: state.into(),
            reachability: Some(reachability.into()),
            basis: Some("transcript".into()),
            last_activity_age_s: Some(12.0),
            last_event_at: None,
            last_message: None,
            observed_model,
            harness_title: None,
        })
    }

    #[test]
    fn progress_ac1_ac2_done_is_parked_working_is_advancing() {
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("done", "reachable").as_ref(),
                "claude",
                None
            ),
            ("parked", "promise")
        );
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("working", "reachable").as_ref(),
                "claude",
                None
            ),
            ("advancing", "transcript-turn")
        );
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("your-move", "reachable").as_ref(),
                "claude",
                None
            ),
            ("awaiting-operator", "operator-turn")
        );
    }

    #[test]
    fn progress_ac3_ac4_refusal_outranks_working_but_never_a_routed_worker() {
        let refused_model = json!({"kind": "observed", "model": "glm-5.2[1m]"});
        assert_eq!(
            progress_from_truth(
                probe_observed("working", "reachable", refused_model.clone()).as_ref(),
                "claude",
                None
            ),
            ("refused", "model-refused"),
            "the refusal must outrank the active working truth state"
        );
        assert_eq!(
            progress_from_truth(
                probe_observed("working", "reachable", refused_model).as_ref(),
                "claude",
                Some("/x/route-settings/ab12.json")
            ),
            ("advancing", "transcript-turn"),
            "a deliberately routed worker must never be condemned"
        );
    }

    #[test]
    fn progress_ac5_unmeasured_observed_model_kinds_never_refuse() {
        for kind in [
            "no-transcript",
            "not-file-backed",
            "no-model-yet",
            "unreadable",
        ] {
            let (verdict, _) = progress_from_truth(
                probe_observed("working", "reachable", json!({"kind": kind})).as_ref(),
                "claude",
                None,
            );
            assert_ne!(verdict, "refused", "kind={kind} must never refuse");
        }
    }

    #[test]
    fn progress_ac6_stalled_is_unknown_silent_never_parked() {
        assert_eq!(
            progress_from_truth(
                probe_with_verdict("stalled", "reachable").as_ref(),
                "claude",
                None
            ),
            ("unknown", "silent")
        );
    }

    #[test]
    fn progress_deliberately_wedged_open_turn_is_quiet_but_not_advancing() {
        let probe = probe_with_age("working", "reachable", Some(STALE_ATTENTION_S + 1.0));
        assert_eq!(
            rendered_status_from_truth(probe.as_ref()),
            "quiet",
            "the process and reachability axes still say present; the transcript has not moved"
        );
        assert_eq!(
            progress_from_truth(probe.as_ref(), "claude", None),
            ("unknown", "silent"),
            "an open turn with no transcript advance past the window is not progressing"
        );
    }

    #[test]
    fn progress_unreadable_activity_age_is_unknown_never_advancing() {
        assert_eq!(
            progress_from_truth(
                probe_with_age("working", "reachable", None).as_ref(),
                "claude",
                None,
            ),
            ("unknown", "no-evidence")
        );
    }

    #[test]
    fn progress_ac7_unreachable_is_unknown_no_evidence_regardless_of_state() {
        for state in ["working", "done", "your-move", "stalled"] {
            assert_eq!(
                progress_from_truth(
                    probe_with_verdict(state, "unreachable").as_ref(),
                    "claude",
                    None
                ),
                ("unknown", "no-evidence"),
                "state={state}"
            );
        }
    }

    #[test]
    fn progress_ac12_fr_a_probe_with_no_reachability_verdict_is_unknown_no_evidence() {
        // The compatibility fallback (a `fno` too old to emit the verdict):
        // `probe()` carries `reachability: None`. An unmeasured row has no
        // progress state to report, so this must never panic and must never
        // read a stale `state` as an active truth-state arm.
        assert_eq!(
            progress_from_truth(probe("working").as_ref(), "claude", None),
            ("unknown", "no-evidence")
        );
        assert_eq!(
            progress_from_truth(None, "claude", None),
            ("unknown", "no-evidence")
        );
    }

    fn test_ctx(home: AgentsHome, worker_bin: PathBuf) -> Ctx {
        Ctx {
            home,
            emitter: EventEmitter::new(std::path::PathBuf::from("/dev/null"), "daemon"),
            opts: DaemonOptions {
                idle_exit: Duration::from_secs(1800),
                worker_bin,
                reconcile_on_start: true,
                agents_config_cwd: PathBuf::from("/dev/null"),
                // Off in tests: a unit test must never spawn a real `fno inbox notify`.
                notify_on_blocked: false,
                notify_on_done: false,
            },
            started_at: std::time::Instant::now(),
            exe_fingerprint: crate::drift::ExeFingerprint::current(),
            pid_start_time: process_start_time(std::process::id()),
            pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
            codex_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
                agents_config_cwd: PathBuf::from("/dev/null"),
                // Off in tests: a unit test must never spawn a real `fno inbox notify`.
                notify_on_blocked: false,
                notify_on_done: false,
            },
            started_at: std::time::Instant::now(),
            exe_fingerprint: crate::drift::ExeFingerprint::current(),
            pid_start_time: process_start_time(std::process::id()),
            pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
            codex_threads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
        let p = PathBuf::from(format!("/tmp/fnosb{tag}{}_{n}", std::process::id()));
        let home = AgentsHome::at(&p);
        home.ensure_root().unwrap();
        home
    }

    /// Seed a held-stream-thread registry row (claude + full UUID + Live).
    fn seed_stream_row(home: &AgentsHome, name: &str, short_id: &str) {
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                substrate: None,
                node: None,
                spawned_by_session: None,
                spawned_by_harness: None,
                spawned_by_cwd: None,
                launch_account: None,
                related_session_id: None,
                origin: None,
                name: name.into(),
                short_id: short_id.into(),
                legacy_provider: "claude".into(),
                provider: None,
                model: None,
                model_basis: None,
                effort: None,
                harness: None,
                harness_session_id: None,
                predecessor_session_ids: Vec::new(),
                forked_from_session_id: None,
                route_provider_id: None,
                model_name: None,
                account_record_id: None,
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                session_id: None,
                spawn_trigger: None,
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
                keeper_child_pid: None,
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
                crown_level: None,
                crown_scope: None,
                crown_grantor: None,
                route_settings_path: None,
                fno_id: None,
                delivery_policy: None,
                sandbox_posture: None,
                ..Default::default()
            });
        })
        .unwrap();
    }

    /// A row with no `short_id` and no `harness_session_id` -- the shape
    /// `registry_truth_handle` cannot resolve to anything a truth probe can
    /// find, matching a pane-hosted codex row that never bound a session id.
    fn seed_bare_row(name: &str) -> RegistryEntry {
        RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: name.into(),
            short_id: String::new(),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: Some("codex".into()),
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: None,
            spawn_trigger: None,
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
            created_at: "2026-06-09T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            keeper_child_pid: None,
            // x-7bcd: no short_id/harness_session_id (leg 3) and no pid (leg
            // 1) is the whole point of this fixture -- give it leg 2 instead
            // so the write-time guard passes without disturbing that intent.
            log_path: Some(format!("/tmp/{name}.log")),
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: Some(state::MuxRef {
                session: "main".into(),
                pane_id: 1,
            }),
            screen_state: None,
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
            sandbox_posture: None,
            ..Default::default()
        }
    }

    /// The shared key-set contract. `handle_list` -- NOT Python's
    /// `serialize_entry` -- is what serves `fno agents list`, and it had stayed
    /// pinned to the pre-v10 key set: no `harness`, no `harness_session_id`, no
    /// `mux`. A peer agent read that surface and nearly filed a wrong diagnosis
    /// onto two nodes because two live pane-hosted workers looked unhosted.
    ///
    /// The guard has to live HERE. The `render_list_json` key assertion in
    /// bin/client.rs cannot catch this: the client passes daemon rows through
    /// verbatim, so that test only asserts against a row it built itself.
    ///
    /// `include_str!` is compile-time, so deleting or moving the contract file
    /// breaks the build rather than silently disarming the check.
    #[test]
    fn watch_serves_on_connect_and_only_on_change() {
        // The subscription contract: connect serves
        // the full document; the same (mtime, len) version answers "unchanged"
        // without a document; any write moves the stamp and the SAME `since`
        // then serves fresh rows. One stat per idle tick is the whole cost.
        let home = short_home("watch-connect");
        seed_stream_row(&home, "w1", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));

        let resp = handle_watch(&ctx, &Request::new(1, "agent.watch", json!({})));
        let first = resp.result().unwrap();
        assert_eq!(first["doc"]["agents"].as_array().unwrap().len(), 1);
        let version = first["version"].clone();

        let resp = handle_watch(
            &ctx,
            &Request::new(2, "agent.watch", json!({"since": version})),
        );
        let again = resp.result().unwrap();
        assert!(
            again["doc"].is_null(),
            "unchanged version serves no document"
        );
        assert_eq!(again["version"], version);

        state::update_registry(&home.registry_json(), |r| {
            r.entries[0].status = AgentStatus::Exited;
        })
        .unwrap();
        let resp = handle_watch(
            &ctx,
            &Request::new(3, "agent.watch", json!({"since": version})),
        );
        let after = resp.result().unwrap();
        assert_ne!(after["version"], version, "a write moves the stamp");
        let doc = after["doc"]["agents"].as_array().unwrap();
        assert_eq!(doc.len(), 1);
        assert_eq!(
            doc[0]["status"],
            json!("exited"),
            "rows are the fresh write"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_row_key_set_matches_shared_contract() {
        const CONTRACT: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/agents-list-row.json"
        ));
        let contract: Value = serde_json::from_str(CONTRACT).expect("contract is valid JSON");
        assert_eq!(
            contract["projection_omissions"],
            json!(["model", "model_basis"]),
            "projection omissions must stay canonical and sorted"
        );
        let mut expected: std::collections::BTreeSet<String> = contract["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect();
        expected.extend(
            contract["rust_only"]["keys"]
                .as_array()
                .expect("rust_only.keys is an array")
                .iter()
                .map(|k| k.as_str().unwrap().to_string()),
        );

        let home = short_home("listcontract");
        seed_stream_row(&home, "worker-contract", "abc12345");
        state::update_registry(&home.registry_json(), |r| {
            let e = &mut r.entries[0];
            // A pane-hosted row holds the mux ref INSTEAD of a transport key
            // (mux XOR worker XOR bg), so short_id is empty -- which is why
            // `session_id` resolves to null for exactly these rows and
            // `harness_session_id` is the only identity they carry.
            e.short_id = String::new();
            e.harness = Some("claude".into());
            e.harness_session_id = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
            e.claude_session_uuid = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
            e.mux = Some(crate::state::MuxRef {
                session: "main".into(),
                pane_id: 10,
            });
            e.crown_level = Some(1);
            e.crown_scope = Some("epic-x".into());
            e.crown_grantor = Some("king".into());
            // A vendor stamp on a claude-hosted row: the exact shape the
            // provider axis exists to describe, and the one the pre-split
            // alias lied about by carrying "claude" here.
            e.provider = Some("zai".into());
            e.effort = Some("xhigh".into());
            e.node = Some("x-cafe".into());
            // (x-7955) AC9-HP: the recorded lane rides verbatim, so a reader
            // can tell a paneless pane row from a thread row.
            e.substrate = Some("thread".into());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
        let result = response.result().unwrap();
        let row = &result["agents"][0];

        let actual: std::collections::BTreeSet<String> =
            row.as_object().unwrap().keys().cloned().collect();
        assert_eq!(actual, expected, "list row key set drifted from contract");
        assert_eq!(
            result["fields_omitted"], contract["projection_omissions"],
            "list envelope omissions drifted from contract"
        );
        // (x-7955) The recorded lane, by VALUE: the projection reads the
        // registry's record, never an inference from mux or thread_id.
        assert_eq!(
            row["substrate"], "thread",
            "the list row carries the recorded substrate"
        );

        // v23 (x-2019), by VALUE and not merely by presence: a substituted
        // row names both values; an unknown request renders null, never a
        // fabricated match. The seeded row carries no request and the probe
        // answers no model, so both keys ride null on the baseline read.
        assert_eq!(row["requested_model"], Value::Null);
        assert_eq!(row["model_substituted"], Value::Null);
        state::update_registry(&home.registry_json(), |r| {
            r.entries[0].requested_model = Some("glm-5.3[1m]".into());
        })
        .unwrap();
        let mut contradicting = probe("working").unwrap();
        contradicting.observed_model = json!({"kind": "observed", "model": "glm-5.3-flash"});
        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|_handle| Some(contradicting.clone())),
        );
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["requested_model"], "glm-5.3[1m]");
        assert_eq!(
            row["model_substituted"],
            json!({"requested": "glm-5.3[1m]", "observed": "glm-5.3-flash"})
        );
        // Suffix-only difference is a MATCH: the marker stays null. The
        // operator's specimen table calls glm-5.3[1m] vs glm-5.3 ok.
        let mut agreeing = probe("working").unwrap();
        agreeing.observed_model = json!({"kind": "observed", "model": "glm-5.3"});
        let response =
            handle_list_with_truth(&ctx, &req, per_handle(|_handle| Some(agreeing.clone())));
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["requested_model"], "glm-5.3[1m]");
        assert_eq!(row["model_substituted"], Value::Null);

        // Presence in the key set is not the bug being guarded: a key that is
        // always null is the same lie in a different shape. Assert the values
        // reach the row.
        assert_eq!(row["harness"], "claude");
        // `provider` carries the stored vendor axis, never a harness (AC8,
        // post x-f273). Emitting it from one serializer only would be worse
        // than emitting it from both: the field would then be present or
        // absent depending on which reader answered.
        assert_eq!(row["provider"], "zai");
        assert_ne!(row["provider"], row["harness"]);
        assert!(row.get("effort").is_some(), "effort key must be emitted");
        assert_eq!(row["effort"], "xhigh");
        assert_eq!(row["node"], "x-cafe");
        assert!(row.get("model").is_none());
        assert_eq!(
            row["harness_session_id"],
            "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
        );
        // The mailbox address, asserted by VALUE and not merely by presence.
        // This row is the exact shape the address column exists for: a pane
        // worker with no transport key, whose only copyable identifier before
        // this key was `name` -- and a name-lane durable write is the largest
        // still-growing category of stranded mail on the bus. The value must
        // equal what `mail drain-self` computes for itself, which is the first
        // eight; the retired `<harness>-<short>` form is refused by the
        // resolver, so emitting it here would advertise an unreachable mailbox.
        assert_eq!(row["address"], "e6f78b98");
        // The pre-fix surface reported this row as having no identity at all:
        // session_id is legitimately null for a pane row (no transport key), so
        // harness_session_id is what has to carry it.
        assert!(row["session_id"].is_null());
        assert_eq!(row["mux"]["session"], "main");
        assert_eq!(row["mux"]["pane_id"], 10);
        assert_eq!(
            row["crown"], "L1 epic-x",
            "same formatter as Python crown_label"
        );
        // The raw crown fields need value assertions too, not just presence:
        // hardcoding either to null passes a key-set check and the bare-row
        // null check, which is the "present but always null" lie again.
        assert_eq!(row["crown_level"], 1);
        assert_eq!(row["crown_scope"], "epic-x");
        assert_eq!(row["crown_grantor"], "king");

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn a_title_probe_that_answered_is_trusted_over_the_stored_baseline() {
        // Served, never stored, applies to ABSENCE too: a probe that answered
        // `harness_title: None` (a rotated transcript carries no agent-name
        // record) must serve None, never the sweep's stale last-seen value;
        // the stored baseline stands only for a row the batch never measured.
        let home = short_home("title-serving");
        seed_stream_row(&home, "worker-title", "abc12345");
        state::update_registry(&home.registry_json(), |r| {
            let e = &mut r.entries[0];
            e.harness = Some("claude".into());
            e.harness_session_id = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
            e.claude_session_uuid = Some("e6f78b98-e594-47ed-ad81-84f8a78b8bb7".into());
            e.harness_title = Some("old-title".into());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));
        let uuid = "e6f78b98-e594-47ed-ad81-84f8a78b8bb7";

        // The probe ANSWERED, and answered no title: serve absence.
        let mut answered = probe_with_verdict("working", "alive").unwrap();
        answered.harness_title = None;
        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(move |h| (h == uuid).then(|| answered.clone())),
        );
        let row = &response.result().unwrap()["agents"][0];
        assert!(
            row["harness_title"].is_null(),
            "a probe that answered None must serve None, got {row}"
        );

        // The probe never answered (unmeasured row): the stored baseline stands.
        let response = handle_list_with_truth(&ctx, &req, per_handle(|_| None));
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(
            row["harness_title"], "old-title",
            "an unmeasured row is served the stored last-seen title"
        );

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn row_contradiction_fixture_matches_python_projection() {
        const FIXTURE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/agents-row-contradiction.json"
        ));
        let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
        let now = chrono::DateTime::parse_from_rfc3339(
            fixture["now"].as_str().expect("fixture now is a string"),
        )
        .expect("fixture now is a timestamp")
        .with_timezone(&chrono::Utc);
        for case in fixture["cases"].as_array().expect("cases is an array") {
            let mut row = case["row"].as_object().expect("row is an object").clone();
            apply_row_contradiction(&mut row, now);
            for (key, expected) in case["expected"].as_object().expect("expected is an object") {
                assert_eq!(row.get(key), Some(expected), "case={}", case["name"]);
            }
        }
    }

    /// The reachability EVIDENCE reaches the row, not just the verdict the
    /// rendered word was picked from.
    ///
    /// `fno agents list` auto-routes here whenever an installed binary is
    /// present, so this is the projection nearly every reader gets -- and both
    /// `peek` and the census comment in this file send a reader to `fno agents
    /// list` for exactly these fields. Emitting them Python-side only left the
    /// documented evidence missing from the default path: the guard-on-one-of-N
    /// shape, in the fix for a guard-on-one-of-N bug.
    ///
    /// The key-set contract above cannot catch this on its own, because a
    /// hardcoded null satisfies it.
    #[test]
    fn list_row_carries_the_reachability_evidence() {
        let home = short_home("listevidence");
        seed_stream_row(&home, "worker-evidence", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|_handle| probe_with_verdict("working", "reachable")),
        );
        let row = &response.result().unwrap()["agents"][0];

        assert_eq!(row["reachability"], "reachable");
        assert_eq!(row["basis"], "transcript");
        assert_eq!(row["last_activity_age_s"], 12.0);
        // The stamp and the LAST-turn text ride the same probe: a
        // hard-coded null would satisfy the key-set contract while hiding the
        // wedged-worker signal the pair exists to expose.
        assert_eq!(row["last_event_at"], "2026-08-15T17:00:00+00:00");
        assert_eq!(
            row["last_message"],
            "Still growing (101 lines, 26 percent through the pytest run)"
        );

        // A probe that did not answer leaves all five null. That is NOT the
        // same as `no-evidence`, which is a verdict this emitter must never
        // invent on the probe's behalf.
        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
        let row = &response.result().unwrap()["agents"][0];
        assert!(row["reachability"].is_null());
        assert!(row["basis"].is_null());
        assert!(row["last_activity_age_s"].is_null());
        assert!(row["last_event_at"].is_null());
        assert!(row["last_message"].is_null());

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_row_live_worker_with_null_activity_age_has_unknown_progress() {
        let home = short_home("listnullage");
        seed_stream_row(&home, "worker-null-age", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|_handle| probe_with_age("working", "reachable", None)),
        );
        let row = &response.result().unwrap()["agents"][0];

        assert!(row["last_activity_age_s"].is_null());
        assert_eq!(row["progress"], "unknown");
        assert_eq!(row["progress_basis"], "no-evidence");

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_progress_filter_is_independent_from_status() {
        let home = short_home("listprogressfilter");
        seed_stream_row(&home, "worker-progress", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));

        let parked = Request::new(1, "agent.list", json!({"progress": "parked"}));
        let response = handle_list_with_truth(
            &ctx,
            &parked,
            per_handle(|_handle| probe_with_verdict("done", "reachable")),
        );
        assert_eq!(
            response.result().unwrap()["agents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let advancing = Request::new(2, "agent.list", json!({"progress": "advancing"}));
        let response = handle_list_with_truth(
            &ctx,
            &advancing,
            per_handle(|_handle| probe_with_verdict("done", "reachable")),
        );
        assert!(response.result().unwrap()["agents"]
            .as_array()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The row reports the model the worker is ACTUALLY answering as, taken
    /// from the same family-1 probe that produced `status`.
    ///
    /// The projection orders rows by evidence of neglect, pinned to the
    /// shared fixture: the same file the mux ranker (crates/fno) and the
    /// Python serializer (cli) assert against. The three cannot share code -
    /// the crates do not link and the CLI is Python - so this file is the
    /// contract that keeps the three orders identical. `include_str!` is
    /// compile-time, so deleting or moving the fixture breaks the build
    /// rather than silently disarming the check.
    #[test]
    fn list_rows_sort_in_the_shared_attention_order() {
        const FIXTURE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/agents-attention-order.json"
        ));
        let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
        let mut rows: Vec<Value> = fixture["rows"]
            .as_array()
            .expect("rows is an array")
            .clone();
        rows.sort_by(|a, b| attention_sort_key(a).cmp(&attention_sort_key(b)));
        let got: Vec<&str> = rows
            .iter()
            .map(|r| r["name"].as_str().expect("row has a name"))
            .collect();
        let expected: Vec<&str> = fixture["expected_order"]
            .as_array()
            .expect("expected_order is an array")
            .iter()
            .map(|v| v.as_str().expect("order entry is a string"))
            .collect();
        assert_eq!(got, expected);
    }

    /// Both list emitters derive this from ONE resolver -- Python's
    /// `session_truth.observed_model`, which the daemon reaches through the
    /// `fno agents truth --json` probe it already runs per row -- so neither
    /// side can report a different model than the other for the same worker
    /// (AC9-CON). The Python half of that binding is asserted in
    /// cli/tests/agents/test_cli_list_logs.py.
    #[test]
    fn list_row_carries_the_observed_model_from_the_truth_probe() {
        let home = short_home("listobserved");
        seed_stream_row(&home, "worker-zai", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|_handle| {
                Some(crate::truth_probe::TruthProbe {
                    state: "working".into(),
                    reachability: Some("reachable".into()),
                    basis: Some("transcript".into()),
                    last_activity_age_s: Some(3.5),
                    last_event_at: None,
                    last_message: None,
                    observed_model: json!({
                        "kind": "observed", "model": "glm-5.2", "samples": 300
                    }),
                    harness_title: None,
                })
            }),
        );
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["observed_model"]["model"], "glm-5.2");
        assert_eq!(row["observed_model"]["kind"], "observed");

        // A probe that did not answer must not leave a bare null: an absent
        // value is what an operator correctly reads as proving nothing, which
        // is the exact misreading this field exists to end.
        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["observed_model"], json!({"kind": "no-transcript"}));

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// A codex pane row with no short_id and no harness_session_id resolves
    /// through `registry_truth_handle` to its bare name, which no truth probe
    /// can ever find. The STATUS word is activity, so even a demonstrably live
    /// pid cannot lift an unanswered age: the row reads `unknown`, the same
    /// word the Python list lane renders for it.
    #[test]
    fn list_status_is_unknown_for_an_unresolvable_row_even_with_a_confirmed_live_pid() {
        let home = short_home("listlivepid");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = seed_bare_row("cx-x-e14b");
            e.pid = Some(std::process::id());
            e.pid_start_time = process_start_time(std::process::id());
            r.entries.push(e);
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["status"], "unknown");

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The pid census does not reach the STATUS word at all: with no probe
    /// answer and no live pid, the row also reads `unknown` (the reachability
    /// fields still carry the pid verdict on their own axis).
    #[test]
    fn list_status_stays_unknown_for_an_unresolvable_row_with_no_confirmed_live_pid() {
        let home = short_home("listnolivepid");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = seed_bare_row("cx-dead");
            e.pid = Some(0x7fff_fff0); // not a live process
            r.entries.push(e);
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| None));
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["status"], "unknown");

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// An opencode row resolves `session_id` from `harness_session_id`, the only
    /// place its id is persisted. Without the arm it fell through to the generic
    /// `session_id`, which is Rust-set only and so null for every Python-written
    /// row -- the same "reports absent when the data exists" defect this row
    /// projection was just fixed for, one field over.
    #[test]
    fn list_row_resolves_opencode_session_id_from_harness_session_id() {
        let home = short_home("listopencode");
        seed_stream_row(&home, "worker-opencode", "abc12345");
        state::update_registry(&home.registry_json(), |r| {
            let e = &mut r.entries[0];
            e.harness = Some("opencode".into());
            e.harness_session_id = Some("oc-sess-9f2".into());
            e.session_id = None;
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
        let result = response.result().unwrap();
        let row = &result["agents"][0];

        assert_eq!(row["harness"], "opencode");
        assert_eq!(row["session_id"], "oc-sess-9f2");

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// An empty crown scope renders `?`, not a trailing space. Python tests the
    /// scope for falsiness (`self.crown_scope or '?'`), so matching only on None
    /// would diverge on the empty string -- and nothing else covers that leg.
    #[test]
    fn list_row_crown_label_falls_back_on_an_empty_scope() {
        let home = short_home("listcrownempty");
        seed_stream_row(&home, "worker-crown", "abc12345");
        state::update_registry(&home.registry_json(), |r| {
            let e = &mut r.entries[0];
            e.crown_level = Some(1);
            e.crown_scope = Some(String::new());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
        let result = response.result().unwrap();

        assert_eq!(result["agents"][0]["crown"], "L1 ?");

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// A row with no pane, no crown and no captured session id emits those keys
    /// as null rather than omitting them -- consumers key off a stable shape.
    #[test]
    fn list_row_emits_absent_optional_fields_as_null() {
        let home = short_home("listnulls");
        seed_stream_row(&home, "worker-bare", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, per_handle(|_handle| probe("working")));
        let result = response.result().unwrap();
        let row = &result["agents"][0];

        // Index-then-is_null would also pass for an ABSENT key (serde_json
        // returns Null for a missing index), which is the very defect being
        // guarded. Assert presence first, then the value.
        let obj = row.as_object().unwrap();
        for key in ["mux", "crown", "crown_level"] {
            assert!(obj.contains_key(key), "row omits key: {key}");
            assert!(obj[key].is_null(), "key {key} should be null on a bare row");
        }

        std::fs::remove_dir_all(home.root()).ok();
    }

    fn stream_identity(short_id: &str) -> Value {
        json!({
            "harness": "claude",
            "session_id": format!("uuid-{short_id}"),
            "short_id": short_id,
            "created_at": "2026-06-09T00:00:00Z",
        })
    }

    fn switchboard_params(
        to: &str,
        to_short: &str,
        from: &str,
        from_short: Option<&str>,
        body: &str,
    ) -> Value {
        let mut params = json!({
            "to": to,
            "from": from,
            "body": body,
            "mirror": from_short.is_some(),
            "recipient_identity": stream_identity(to_short),
        });
        if let Some(short_id) = from_short {
            params["from_identity"] = stream_identity(short_id);
        }
        params
    }

    #[test]
    fn list_renders_family1_truth_instead_of_stored_registry_status() {
        let home = short_home("listtruth");
        seed_stream_row(&home, "worker-list", "abc12345");
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries[0].status = AgentStatus::Orphaned;
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"status": "writing"}));

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|_handle| probe_with_verdict("working", "reachable")),
        );
        let result = response.result().unwrap();
        let agents = result["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["status"], "writing");

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_queries_family1_by_session_identity_not_custom_name() {
        let home = short_home("listidentity");
        seed_stream_row(&home, "custom-worker-name", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"status": "writing"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|handle| {
                seen.borrow_mut().push(handle.to_string());
                probe_with_verdict("working", "reachable")
            }),
        );

        assert!(response.result().is_some());
        assert_eq!(seen.into_inner(), vec!["uuid-abc12345"]);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_queries_pidless_row_by_bare_canonical_handle() {
        let home = short_home("listpidless");
        seed_stream_row(&home, "custom-worker-name", "unused");
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries[0].short_id.clear();
            registry.entries[0].harness = Some("codex".into());
            registry.entries[0].harness_session_id =
                Some("019f8ff2-1111-2222-3333-444444444444".into());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"status": "writing"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|handle| {
                seen.borrow_mut().push(handle.to_string());
                probe_with_verdict("working", "reachable")
            }),
        );

        assert!(response.result().is_some());
        assert_eq!(
            seen.into_inner(),
            vec!["019f8ff2-1111-2222-3333-444444444444"]
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_queries_non_claude_row_by_transcript_identity() {
        let home = short_home("listnonclaude");
        seed_stream_row(&home, "custom-worker-name", "transport");
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries[0].harness = Some("codex".into());
            registry.entries[0].harness_session_id =
                Some("019f8ff2-1111-2222-3333-444444444444".into());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"status": "writing"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|handle| {
                seen.borrow_mut().push(handle.to_string());
                probe_with_verdict("working", "reachable")
            }),
        );

        assert!(response.result().is_some());
        assert_eq!(
            seen.into_inner(),
            vec!["019f8ff2-1111-2222-3333-444444444444"]
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_applies_cheap_filters_before_family1_subprocesses() {
        let home = short_home("listprefilter");
        seed_stream_row(&home, "claude-worker", "aaaaaaaa");
        seed_stream_row(&home, "codex-worker", "bbbbbbbb");
        state::update_registry(&home.registry_json(), |registry| {
            registry.entries[1].harness = Some("codex".into());
            registry.entries[1].harness_session_id =
                Some("bbbbbbbb-1111-2222-3333-444444444444".into());
            // The provider filter reads the v15+ vendor axis: stamp the row
            // so it matches below. entries[0] stays unstamped, so a null
            // provider contributes to no vendor filter.
            registry.entries[1].provider = Some("openai".into());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"provider": "openai"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(
            &ctx,
            &req,
            per_handle(|handle| {
                seen.borrow_mut().push(handle.to_string());
                probe("working")
            }),
        );

        assert!(response.result().is_some());
        assert_eq!(
            seen.into_inner(),
            vec!["bbbbbbbb-1111-2222-3333-444444444444"]
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_pays_exactly_one_batch_call_for_the_whole_page() {
        // The x-0d93 fix, asserted where it is spent: N rows used to cost N
        // Python interpreter cold starts (780 ms each) on every `fno agents
        // list`. Asserted against the RAW seam, never `per_handle` -- an
        // adapter that fans a batch back out renders identically whether the
        // handler called it once or once per row, so only a direct call count
        // can tell the two apart.
        let home = short_home("listbatchonce");
        let rows = 24;
        for i in 0..rows {
            seed_stream_row(&home, &format!("worker-{i:02}"), &format!("{i:02}aaaaaa"));
        }
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"all": true}));
        let calls = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(&ctx, &req, |handles: &[String]| {
            calls.borrow_mut().push(handles.to_vec());
            handles
                .iter()
                .map(|h| (h.clone(), probe("working").unwrap()))
                .collect()
        });

        let calls = calls.into_inner();
        assert_eq!(calls.len(), 1, "one page, one batch");
        assert_eq!(calls[0].len(), rows, "every filtered row rides that batch");
        let entries = response.result().unwrap()["agents"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(entries, rows);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_row_the_batch_did_not_answer_renders_exactly_as_an_unanswered_row() {
        // A handle absent from the batch map must be indistinguishable from the
        // per-row path's `None`: null triple, no invented model. Anything else
        // and the batch would be reporting a reading it never took.
        let home = short_home("listbatchpartial");
        seed_stream_row(&home, "answered", "aaaaaaaa");
        seed_stream_row(&home, "unanswered", "bbbbbbbb");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"all": true}));

        let response = handle_list_with_truth(&ctx, &req, |handles: &[String]| {
            // Answers for the first handle only; the second is simply missing.
            handles
                .iter()
                .take(1)
                .map(|h| (h.clone(), probe("working").unwrap()))
                .collect()
        });

        let agents = response.result().unwrap()["agents"].clone();
        let rows = agents.as_array().unwrap();
        let missing = rows
            .iter()
            .find(|r| r["name"] == "unanswered")
            .expect("the unanswered row still renders");
        assert!(missing["reachability"].is_null());
        assert!(missing["basis"].is_null());
        assert!(missing["last_activity_age_s"].is_null());
        assert_eq!(missing["observed_model"]["kind"], "no-transcript");
        std::fs::remove_dir_all(home.root()).ok();
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
        // Phase 1: the socket file appears (the worker has bound).
        let bind_start = std::time::Instant::now();
        while !sock.exists() && bind_start.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            sock.exists(),
            "stream worker socket never appeared for {short_id}"
        );
        // Phase 2: the worker actually accepts and answers a ping. sock.exists()
        // is bind, not readiness - a bound-but-starved worker (its own OS thread
        // + bash subprocess compete for cores under --test-threads=32) makes the
        // caller's 2s liveness probe time out and the test flake as
        // delivered:false ("not-a-live-stream-thread"). Wait for a ping before
        // handing the socket back, so the caller always sees a warm worker.
        // Capture the successful probe rather than re-probing in the assert: a
        // second independent 2s probe can itself time out under the same
        // CPU-starvation that made us wait, flaking the helper after readiness
        // was already established.
        let live_start = std::time::Instant::now();
        let mut live = false;
        while live_start.elapsed() < Duration::from_secs(30) {
            if is_live_stream_thread(&sock).await {
                live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            live,
            "stream worker socket appeared but never answered a ping for {short_id}"
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
        // thing, so goose (still genuinely unhosted) is the example now.
        assert_eq!(
            provider_readiness_detector("goose").provider_name(),
            "goose"
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
        assert_eq!(e.harness_name(), "claude");
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
        // Hermetic claims: point `fno agents claim` at the test home so the real
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
        assert_eq!(res["harness"], "claude");
        assert_eq!(res["status"], "live");
        assert_eq!(res["lane"], "stream");

        let reg = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry readable");
        let row = reg.find("cl").expect("adopted row missing");
        assert_eq!(row.harness_name(), "claude");
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
        let reg = load_registry_offloaded(home.registry_json())
            .await
            .expect("registry readable");
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
            switchboard_params("B", "swB", "A", Some("swA"), "hello"),
        );
        let resp = handle_switchboard(&ctx, &req).await;
        let res = resp.result().expect("switchboard errored");
        assert_eq!(res["delivered"], true, "not delivered: {res:?}");
        assert_eq!(res["reply"], "reply-text");
        assert_eq!(res["is_error"], false);
        assert_eq!(res["receipt"], true, "user-echo receipt not observed");
        assert_eq!(res["mirrored"], true, "B's reply was not mirrored into A");
        assert_eq!(res["identity_verified"], true);

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
                switchboard_params("B", "swB", "ghost", None, "first"),
            ),
        )
        .await;
        assert_eq!(r1.result().expect("hop1")["reply"], "reply-1");

        let r2 = handle_switchboard(
            &ctx,
            &Request::new(
                2,
                "agent.switchboard",
                switchboard_params("B", "swB", "ghost", None, "second"),
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
            switchboard_params("B", "swB", "A", None, "hi"),
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
            switchboard_params("B", "swB", "ghost", None, "hi"),
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
            switchboard_params("nope", "swNope", "A", None, "hi"),
        );
        let resp = handle_switchboard(&ctx, &req).await;
        if let crate::protocol::ResponsePayload::Err(ref e) = resp.payload {
            assert_eq!(e.code, ErrorCode::AgentNotFound);
        } else {
            panic!("expected AgentNotFound, got {resp:?}");
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_refuses_replaced_recipient_identity() {
        let home = short_home("replaced");
        seed_stream_row(&home, "victim", "swB");
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
        let mut params = switchboard_params("victim", "swB", "ghost", None, "secret");
        params["recipient_identity"]["session_id"] = json!("uuid-swA");

        let response =
            handle_switchboard(&ctx, &Request::new(1, "agent.switchboard", params)).await;
        let result = response.result().expect("identity mismatch is a demotion");
        assert_eq!(result["delivered"], false);
        assert_eq!(result["reason"], "recipient-identity-changed");
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_failed_drive_does_not_orphan_restamped_recipient() {
        let home = short_home("failedrestamp");
        seed_stream_row(&home, "B", "swB");
        let turn_started = home.root().join("turn-started");
        let restamp_done = home.root().join("restamp-done");
        let script = format!(
            r#"
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s1"}}'
while IFS= read -r line; do
  touch '{}'
  while [ ! -f '{}' ]; do sleep 0.01; done
  exit 1
done
"#,
            turn_started.display(),
            restamp_done.display()
        );
        let _b = start_stream_worker(&home, "swB", &script).await;
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent-worker"));

        let registry_path = home.registry_json();
        let restamp_signal = turn_started.clone();
        let restamp_complete = restamp_done.clone();
        let restamp = tokio::spawn(async move {
            let start = Instant::now();
            while !restamp_signal.exists() && start.elapsed() < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(restamp_signal.exists(), "drive never reached the worker");
            state::update_registry(&registry_path, |registry| {
                let row = registry.find_mut("B").expect("recipient row missing");
                row.short_id = "swC".into();
                row.harness_session_id = Some("uuid-replacement".into());
                row.claude_session_uuid = Some("uuid-replacement".into());
                row.created_at = "2026-06-09T00:00:01Z".into();
                row.status = AgentStatus::Live;
            })
            .unwrap();
            std::fs::write(restamp_complete, b"done\n").unwrap();
        });

        let response = handle_switchboard(
            &ctx,
            &Request::new(
                1,
                "agent.switchboard",
                switchboard_params("B", "swB", "ghost", None, "fail after receipt"),
            ),
        )
        .await;
        restamp.await.unwrap();
        let result = response.result().expect("failed drive is a demotion");
        assert_eq!(result["delivered"], false);

        let registry = state::load_registry(&home.registry_json()).unwrap();
        let replacement = registry.find("B").expect("replacement row missing");
        assert_eq!(replacement.status, AgentStatus::Live);
        assert_eq!(
            replacement.harness_session_id.as_deref(),
            Some("uuid-replacement")
        );
        let events = read_events(&home);
        assert!(
            events.iter().any(|event| {
                event["type"] == "agent_deliver_status_write_failed"
                    && event["data"]["name"] == "B"
                    && event["data"]["reason"] == "recipient-identity-changed"
            }),
            "identity-CAS failure event missing: {events:?}"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_requires_recipient_identity() {
        let home = short_home("identity-required");
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
        let response = handle_switchboard(
            &ctx,
            &Request::new(
                1,
                "agent.switchboard",
                json!({"to": "victim", "from": "ghost", "body": "secret", "mirror": false}),
            ),
        )
        .await;
        assert_eq!(
            response.error().expect("missing identity must fail").code,
            ErrorCode::InvalidParams
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_v2_routes_to_identity_guard() {
        let home = short_home("v2route");
        let ctx = Arc::new(test_ctx(home.clone(), PathBuf::from("/nonexistent-worker")));
        let response = dispatch_agent(
            &ctx,
            &Request::new(
                1,
                "agent.switchboard_v2",
                json!({"to": "victim", "from": "ghost", "body": "secret"}),
            ),
        )
        .await;
        let error = response.error().expect("missing identity must fail");
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert!(error.message.contains("recipient_identity"));
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

    /// The attach lane WITHOUT a harness-owned server (claude) must refuse
    /// with the client-side-lane pointer, never reach the codex app-server
    /// lane: `thread_lane` answers "attach" for claude too, so a bare lane
    /// test would hand a claude thread spawn to codex's app-server.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_spawn_thread_attach_without_server_refuses_with_client_pointer() {
        let home = tmp_home("spawn-thread-attach-client");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "test-agent", "provider": "claude", "substrate": "thread"}),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidParams);
                assert!(
                    e.message.contains("--substrate thread"),
                    "attach-without-server must point at the client-side lane; got: {}",
                    e.message
                );
                assert!(
                    !e.message.contains("retired at G4"),
                    "this refusal is a lane split, not PTY retirement; got: {}",
                    e.message
                );
            }
            _ => panic!("expected refusal for an attach lane without a server"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// A keeper-lane harness (agy) refuses naming fno's keeper process; the
    /// text carries no mux pointer and no daemon-PTY retirement claim.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_spawn_thread_keeper_lane_refuses_naming_keeper() {
        let home = tmp_home("spawn-thread-keeper");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "test-agent", "provider": "agy", "substrate": "thread"}),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidParams);
                assert!(
                    e.message.contains("keeper"),
                    "keeper-lane refusal must name the keeper process; got: {}",
                    e.message
                );
                assert!(
                    !e.message.contains("mux"),
                    "keeper-lane refusal is not a PTY-retirement pointer; got: {}",
                    e.message
                );
                assert!(
                    !e.message.contains("retired at G4"),
                    "keeper-lane refusal must not recycle the G4 message; got: {}",
                    e.message
                );
            }
            _ => panic!("expected refusal for a keeper-lane harness"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// An unknown harness on the thread substrate refuses via the contract
    /// error rather than routing to any lane.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_spawn_thread_unknown_harness_refuses() {
        let home = tmp_home("spawn-thread-unknown");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "test-agent", "provider": "nonexistent-provider", "substrate": "thread"}),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(
                    e.code,
                    ErrorCode::InvalidParams,
                    "unknown harness on the thread substrate must refuse"
                );
                assert!(
                    e.message.contains("unknown harness"),
                    "refusal must carry the contract error; got: {}",
                    e.message
                );
            }
            _ => panic!("expected refusal for an unknown harness"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The destination's own precondition: a provider the codex lane cannot
    /// serve refuses loudly instead of silently starting a codex thread under
    /// the caller's name. Pinned by calling the lane directly, because no
    /// packaged row today answers attach-with-server except codex - this is
    /// the guard a SECOND such row meets until its destination is wired.
    #[tokio::test(flavor = "current_thread")]
    async fn codex_thread_lane_refuses_a_provider_it_cannot_serve() {
        let home = tmp_home("codex-lane-wrong-provider");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "test-agent", "provider": "claude", "substrate": "thread"}),
        );
        let resp =
            spawn_codex_thread_lane(&ctx, &req, "test-agent", Path::new("/tmp"), "claude").await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidParams);
                assert!(
                    e.message.contains("needs its own thread destination"),
                    "the wrong-harness guard must name the missing destination; got: {}",
                    e.message
                );
            }
            _ => panic!("expected the codex lane to refuse a provider it cannot serve"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The thread route's provider default is `codex`, matching the client's
    /// daemon-bound predicate: a thread spawn with no provider reaches the
    /// app-server lane, never a refusal (green gate, mute worker - the
    /// defaults-must-match note in client.rs run()).
    #[tokio::test(flavor = "current_thread")]
    async fn handle_spawn_thread_absent_provider_defaults_to_codex_lane() {
        with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
            let home = tmp_home("spawn-thread-default-provider");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let worktree = home.root().join("worktree");
            std::fs::create_dir_all(&worktree).unwrap();
            let req = Request::new(
                1,
                "agent.spawn",
                json!({
                    "name": "t",
                    "substrate": "thread",
                    "cwd": worktree.to_string_lossy(),
                    "message": "seed turn",
                }),
            );
            let resp = handle_spawn(&ctx, &req).await;
            assert!(
                resp.result().is_some(),
                "absent provider must default to codex and reach the thread lane: {resp:?}"
            );
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    // --- codex thread lane: actor-driven ask / stop (the x-de10 probes) ---
    //
    // These three are the make-it-fail probes for the concurrency rewrite:
    // each FAILS against the old Arc<Mutex<CodexThread>> shape (verified by
    // running them on the pre-rewrite daemon) and passes against the actor.

    // PATH + ask-wait env serialization across these tests AND every other
    // PATH-mutating test in the lib (provider.rs, client_verbs.rs): one shared
    // mutex, never nested. The fakes install `codex` on PATH and one test
    // retunes FNO_CODEX_ASK_WAIT_MS; a concurrent `set_var` from an unrelated
    // test would make the child inherit a broken PATH (exit 127).

    /// Point `CODEX_HOME` at a fake shared app-server daemon for the duration
    /// of `body`. The fake lives in [`crate::codex_fake_daemon`]: one
    /// implementation of the protocol, shared with the integration tests.
    async fn with_fake_codex_daemon(
        behavior: crate::codex_fake_daemon::Behavior,
        body: impl std::future::Future<Output = ()>,
    ) {
        let _guard = crate::path_test_guard();
        let _daemon = crate::codex_fake_daemon::FakeDaemon::start(behavior);
        body.await;
    }

    /// Block until the worker's actor reports a DRIVING turn, or fail.
    ///
    /// The tests below used a fixed 300ms sleep, which is a bet that the seed
    /// turn started by then. Under a loaded parallel suite it does not always
    /// hold, and losing it does not make a test slower, it makes it WRONG: an
    /// interrupt arriving before the turn starts reads `NoTurnInFlight`, so
    /// `stop` correctly reports `no-turn` and the assertion for `interrupted`
    /// fails. Waiting on the actor's own turn id removes the bet.
    async fn await_driving_turn(ctx: &Ctx, name: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let handle = ctx.codex_threads.lock().await.get(name).cloned();
            if let Some(turn) = handle.and_then(|handle| handle.current_turn_id()) {
                return turn;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no turn ever started driving for {name}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Block until an `agent_ask_done` is on disk, then return every event.
    ///
    /// The two callers slept a fixed 400ms or 1300ms and then counted. That is
    /// a bet that the fake's turn finished AND its emit reached the file inside
    /// the window. CI lost it: `switchboard_to_codex_thread_delivers_on_steering_ack_mid_turn`
    /// read ZERO done events on a loaded runner while passing on every local
    /// run. A completion is an observable marker, so wait for the marker.
    ///
    /// Counting `== 1` right after the first one lands is still sound: both
    /// callers make exactly two submits and have already asserted upstream
    /// that the two shared one turn id, so nothing can start a second turn
    /// after this returns.
    async fn await_ask_done(home: &AgentsHome) -> Vec<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let events = read_events(home);
            if events.iter().any(|e| e["type"] == "agent_ask_done") {
                return events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no agent_ask_done ever landed: {events:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// Spawn a codex thread worker through the real handle_spawn and return
    /// its response.
    async fn spawn_codex_thread_for_test(ctx: &Ctx, home: &AgentsHome, seed: &str) -> Response {
        let worktree = home.root().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let req = Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "t",
                "provider": "codex",
                "substrate": "thread",
                "cwd": worktree.to_string_lossy(),
                "message": seed,
            }),
        );
        handle_spawn(ctx, &req).await
    }

    /// AC4 make-it-fail probe: an ask arriving while the SEED turn is driving
    /// STEERS into it. Old mutex shape: the ask queued behind the whole seed
    /// turn and drove a SECOND turn - this asserted reply would read REPLY-2
    /// and two agent_ask_done events would land. Actor: one shared turn, one
    /// event, the ask returns the seed turn's own reply.
    #[tokio::test(flavor = "current_thread")]
    async fn codex_thread_ask_while_driving_steers_instead_of_queueing() {
        with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
            let home = tmp_home("codex-steer");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "seed turn").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");

            let ask = handle_ask(
                &ctx,
                &Request::new(2, "agent.ask", json!({"name": "t", "message": "follow-up"})),
            )
            .await;
            let res = ask.result().expect("ask errored");
            assert_eq!(
                res["reply"], "REPLY-1",
                "the follow-up must ride the seed turn, not drive a second one: {res:?}"
            );

            // Exactly ONE completed turn: the seed and the steered ask share
            // it, so exactly one agent_ask_done event fires.
            let events = await_ask_done(&home).await;
            let done = events
                .iter()
                .filter(|e| e["type"] == "agent_ask_done")
                .count();
            assert_eq!(done, 1, "one shared turn must emit one event: {events:?}");
            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// AC10 (x-296f): a SEEDLESS codex thread spawn takes the warmup turn, so
    /// a rollout exists and the worker is attachable from its first seconds.
    /// The positive marker is the fake daemon's own received frame: a
    /// `turn/start` carrying the warmup text. `thread/start` alone writes no
    /// rollout and a harness resolves a session BY that rollout, so without
    /// the warmup the first attach dies with "no rollout found for thread id".
    #[tokio::test(flavor = "current_thread")]
    async fn a_seedless_codex_thread_spawn_takes_the_warmup_turn() {
        let behavior = crate::codex_fake_daemon::Behavior::quick();
        let received = std::sync::Arc::clone(&behavior.received);
        with_fake_codex_daemon(behavior, async {
            let home = tmp_home("codex-warmup");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            // Seedless: the spawn request carries no message at all.
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");

            // The seed submit is async in the actor; wait for the frame rather
            // than racing it.
            let turns: Vec<serde_json::Value> = {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let turns: Vec<serde_json::Value> = received
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .iter()
                        .filter(|f| f["method"] == "turn/start")
                        .cloned()
                        .collect();
                    if !turns.is_empty() || std::time::Instant::now() >= deadline {
                        break turns;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            };
            assert_eq!(
                turns.len(),
                1,
                "a seedless spawn takes exactly one warmup turn: {turns:?}"
            );
            assert_eq!(
                turns[0]["params"]["input"][0]["text"], WARMUP_SEED,
                "the warmup is the seed that was submitted: {turns:?}"
            );

            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// The warmup must not double-submit behind a real seed: a spawn that
    /// carries a prompt drives exactly that prompt, verbatim.
    #[tokio::test(flavor = "current_thread")]
    async fn a_seeded_codex_thread_spawn_drives_its_own_seed_only() {
        let behavior = crate::codex_fake_daemon::Behavior::quick();
        let received = std::sync::Arc::clone(&behavior.received);
        with_fake_codex_daemon(behavior, async {
            let home = tmp_home("codex-real-seed");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "do the actual work").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");

            let turns: Vec<serde_json::Value> = {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let turns: Vec<serde_json::Value> = received
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .iter()
                        .filter(|f| f["method"] == "turn/start")
                        .cloned()
                        .collect();
                    if !turns.is_empty() || std::time::Instant::now() >= deadline {
                        break turns;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            };
            assert_eq!(turns.len(), 1, "one seed, one turn: {turns:?}");
            assert_eq!(
                turns[0]["params"]["input"][0]["text"], "do the actual work",
                "a real seed passes through verbatim: {turns:?}"
            );

            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// AC5 + AC6 make-it-fail probe: stop INTERRUPTS the in-flight turn before
    /// reporting stopped and names the interrupt outcome in the response. Old
    /// shape: no `interrupt` key (remove-and-stamp while the turn task still
    /// held an Arc clone), so the `interrupt == "interrupted"` assert fails
    /// there.
    ///
    /// It also pins the ownership claim this lane exists for. The row's pid
    /// is the SHARED daemon's, and that daemon is still running after the
    /// stop. The assertion used to be the opposite (the pid must be GONE),
    /// which is what owning a private app-server per worker looked like.
    #[tokio::test(flavor = "current_thread")]
    async fn codex_thread_stop_interrupts_and_stamps_exited_without_killing_the_daemon() {
        with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::long(), async {
            let home = tmp_home("codex-stop");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
            let registry = load_registry_offloaded(home.registry_json())
                .await
                .expect("registry");
            assert_eq!(
                registry.find("t").and_then(|entry| entry.pid),
                None,
                "a thread row records no pid: it owns no process, and this \
                 field is a liveness surface"
            );
            let daemon_state: Value = serde_json::from_str(
                &std::fs::read_to_string(
                    std::path::PathBuf::from(std::env::var("CODEX_HOME").unwrap())
                        .join("app-server-daemon")
                        .join("app-server.pid"),
                )
                .expect("daemon state"),
            )
            .expect("daemon state json");
            let pid = daemon_state["pid"].as_u64().expect("daemon pid") as u32;

            // Stop mid-turn, once the turn is actually driving.
            await_driving_turn(&ctx, "t").await;
            let stop =
                handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
            let res = stop.result().expect("stop errored");
            assert_eq!(res["stopped"], true, "stop response: {res:?}");
            assert_eq!(
                res["interrupt"], "interrupted",
                "stopped must name the interrupt outcome: {res:?}"
            );

            let registry = load_registry_offloaded(home.registry_json())
                .await
                .expect("registry");
            assert_eq!(
                registry.find("t").map(|e| e.status),
                Some(AgentStatus::Exited)
            );

            // The shared daemon must SURVIVE the stop. Stopping a worker
            // closes one connection; killing the app-server would take every
            // other codex session on the machine with it.
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            assert!(
                alive,
                "stopping a worker killed the shared app-server daemon {pid}"
            );
            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// The zombie-stop probe: an interrupt the daemon never confirms must NOT
    /// report a stop.
    ///
    /// With a private app-server, `kill_on_drop` made every stop terminal, so
    /// `stopped: true` was always true. Against the shared daemon nothing ends
    /// the turn but the interrupt itself, and an unconfirmed one leaves the
    /// model taking that turn in the worker's worktree. Reporting `stopped:
    /// true` there marks the row Exited, hides it from recovery, and discards
    /// the interrupt handle, while the work continues unobserved.
    ///
    /// The fake acks the interrupt and never completes the turn, which is
    /// exactly that state.
    #[tokio::test(flavor = "current_thread")]
    async fn codex_thread_stop_refuses_over_a_turn_the_interrupt_never_settled() {
        let behavior = crate::codex_fake_daemon::Behavior::long().with_interrupt(
            crate::codex_fake_daemon::Interrupt::AckOnly(std::time::Duration::ZERO),
        );
        with_fake_codex_daemon(behavior, async {
            // A Drop guard, not a teardown line: an assertion below panics
            // out of this body, and a leaked bound would silently shorten
            // every later test's interrupt wait in the same process.
            struct BoundGuard;
            impl Drop for BoundGuard {
                fn drop(&mut self) {
                    std::env::remove_var("FNO_CODEX_INTERRUPT_BOUND_MS");
                }
            }
            std::env::set_var("FNO_CODEX_INTERRUPT_BOUND_MS", "1500");
            let _bound = BoundGuard;
            let home = tmp_home("codex-zombie-stop");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
            await_driving_turn(&ctx, "t").await;

            let stop =
                handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
            let res = stop.result().expect("stop errored");
            assert_eq!(
                res["stopped"], false,
                "an unsettled interrupt must not report a stop: {res:?}"
            );
            assert_eq!(
                res["interrupt"], "timeout-turn-still-running",
                "the response names why: {res:?}"
            );

            // The row stays non-terminal, so recovery can still see it, and
            // the handle stays so the live turn keeps an interrupt handle.
            let registry = load_registry_offloaded(home.registry_json())
                .await
                .expect("registry");
            assert_ne!(
                registry.find("t").map(|entry| entry.status),
                Some(AgentStatus::Exited),
                "a refused stop must not stamp the row terminal"
            );
            assert!(
                ctx.codex_threads.lock().await.contains_key("t"),
                "the actor must survive a refused stop; it holds the interrupt handle"
            );

            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// AC3 make-it-fail probe: an ask against a turn longer than the bounded
    /// wait answers `in_flight` with the turn id while the turn keeps running.
    /// Old shape: the ask blocked on the mutex for the whole 30s turn and
    /// returned a completed reply - `status == "in_flight"` fails there.
    #[tokio::test(flavor = "current_thread")]
    async fn codex_thread_ask_returns_in_flight_when_turn_exceeds_bound() {
        with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::long(), async {
            std::env::set_var("FNO_CODEX_ASK_WAIT_MS", "200");
            let home = tmp_home("codex-inflight");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "long seed turn").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
            await_driving_turn(&ctx, "t").await;

            let started = std::time::Instant::now();
            let ask = handle_ask(
                &ctx,
                &Request::new(2, "agent.ask", json!({"name": "t", "message": "status?"})),
            )
            .await;
            let res = ask.result().expect("ask errored");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "the bounded ask must answer near its 200ms bound, took {:?}",
                started.elapsed()
            );
            assert_eq!(res["status"], "in_flight", "in_flight receipt: {res:?}");
            assert!(res["reply"].is_null(), "in_flight reply is null: {res:?}");
            assert_eq!(
                res["turn_id"], "turn-1",
                "the receipt carries the surviving interrupt handle: {res:?}"
            );

            // Stop cleans up: interrupts the still-driving turn and kills it.
            let stop =
                handle_stop(&ctx, &Request::new(3, "agent.stop", json!({"name": "t"}))).await;
            let stop_res = stop.result().expect("stop errored");
            assert_eq!(stop_res["interrupt"], "interrupted");
            std::env::remove_var("FNO_CODEX_ASK_WAIT_MS");
            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// AC8 make-it-fail probe: mail arriving MID-TURN answers delivered on the
    /// STEER ACK (milliseconds) and drives exactly ONE shared turn. Old shape:
    /// the thread fell out of the switchboard as not-a-live-stream-thread, so
    /// `delivered` read false - this assert fails there.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_to_codex_thread_delivers_on_steering_ack_mid_turn() {
        with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
            let home = tmp_home("codex-mail");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "seed turn").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
            let registry = load_registry_offloaded(home.registry_json())
                .await
                .expect("registry");
            let row = registry.find("t").expect("thread row").clone();
            await_driving_turn(&ctx, "t").await;

            let params = json!({
                "to": "t",
                "from": "king",
                "body": "hello thread",
                "mirror": false,
                "recipient_identity": {
                    "harness": "codex",
                    "session_id": row.harness_session_id,
                    "short_id": "",
                    "created_at": row.created_at,
                },
            });
            let started = std::time::Instant::now();
            let resp =
                handle_switchboard(&ctx, &Request::new(4, "agent.switchboard_v2", params)).await;
            let res = resp.result().expect("switchboard errored");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "delivery must answer on the steer ack, took {:?}",
                started.elapsed()
            );
            assert_eq!(res["delivered"], true, "codex mail: {res:?}");
            assert_eq!(res["identity_verified"], true);
            assert_eq!(res["turn_id"], "turn-1", "steered into the shared turn");

            // The body reached the thread: the steered turn carries it, so the
            // completion event names the same single turn.
            let events = await_ask_done(&home).await;
            let done: Vec<_> = events
                .iter()
                .filter(|e| e["type"] == "agent_ask_done")
                .collect();
            assert_eq!(done.len(), 1, "one shared turn: {events:?}");
            assert_eq!(
                done[0]["data"]["turn_id"], "turn-1",
                "the completion must name the turn both submits shared: {events:?}"
            );
            let injected = events.iter().any(|e| {
                e["type"] == "agent_deliver_injected"
                    && e["data"]["transport"] == "switchboard"
                    && e["data"]["provider"] == "codex"
            });
            assert!(injected, "injected event missing: {events:?}");

            // Cleanup: the actor holds a live daemon connection.
            handle_stop(&ctx, &Request::new(5, "agent.stop", json!({"name": "t"}))).await;
            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
    }

    /// AC8 (idle half): mail to an IDLE codex thread starts the turn itself
    /// and answers delivered with that turn id - no pane, no durable demote.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_to_idle_codex_thread_delivers_on_start_ack() {
        with_fake_codex_daemon(crate::codex_fake_daemon::Behavior::quick(), async {
            let home = tmp_home("codex-mail-idle");
            let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent"));
            // No seed: the row is idle at mail time.
            let spawned = spawn_codex_thread_for_test(&ctx, &home, "").await;
            assert!(spawned.result().is_some(), "spawn failed: {spawned:?}");
            let registry = load_registry_offloaded(home.registry_json())
                .await
                .expect("registry");
            let row = registry.find("t").expect("thread row").clone();

            let params = json!({
                "to": "t",
                "from": "king",
                "body": "wake up",
                "mirror": false,
                "recipient_identity": {
                    "harness": "codex",
                    "session_id": row.harness_session_id,
                    "short_id": "",
                    "created_at": row.created_at,
                },
            });
            let resp =
                handle_switchboard(&ctx, &Request::new(4, "agent.switchboard_v2", params)).await;
            let res = resp.result().expect("switchboard errored");
            assert_eq!(res["delivered"], true, "idle codex mail: {res:?}");
            assert_eq!(res["turn_id"], "turn-1", "started the turn: {res:?}");

            handle_stop(&ctx, &Request::new(5, "agent.stop", json!({"name": "t"}))).await;
            ctx.codex_threads.lock().await.remove("t");
            std::fs::remove_dir_all(home.root()).ok();
        })
        .await;
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

    /// A full-session-id token must resolve in handle_ask too: the client
    /// pre-check accepts one, so a name-only lookup here read a live agent as
    /// absent and, with --provider, would auto-spawn a duplicate row for the
    /// same session.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_ask_resolves_a_full_session_id_to_the_named_row() {
        let home = tmp_home("ask-full-id");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let session_id = "12345678-1234-4234-8234-123456789abc";
        let mut row = ask_row("alice", None);
        row.harness = Some("claude".into());
        row.harness_session_id = Some(session_id.into());
        row.short_id = "abcd1234".into();
        row.status = AgentStatus::Live;
        state::update_registry(
            &home.registry_json(),
            |r| -> Result<(), std::convert::Infallible> {
                r.entries.push(row);
                Ok(())
            },
        )
        .unwrap();
        let req = Request::new(
            1,
            "agent.ask",
            json!({
                "name": session_id.to_ascii_uppercase(),
                "message": "hello"
            }),
        );
        let resp = handle_ask(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Ok(_) => {}
            crate::protocol::ResponsePayload::Err(e) => {
                assert!(
                    !e.message.contains("pass --provider"),
                    "a full-id token must resolve to the named row, not read as absent: {}",
                    e.message
                );
            }
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

    /// The `blocked` producer (Notification hook) stores state and
    /// reason exactly like `working`/`done` and gets the same capability flip
    /// -- a `blocked` row is demoted from the scraper by construction, not by
    /// a special case, so a stale screen-manifest verdict can never shadow a
    /// hook-reported Waiting row.
    #[test]
    fn handle_report_blocked_stores_reason_and_clears_screen_state() {
        let home = tmp_home("report-blocked");
        seed_stream_row(&home, "worker-A", "repW");
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
                json!({"session_id": "uuid-repW", "seq": 1, "state": "blocked", "reason": "permission to run rm"}),
            ),
        );
        assert!(!resp.is_err(), "report must return Ok: {resp:?}");
        assert_eq!(resp.result().unwrap()["stored"], true);

        let reg = state::load_registry(&home.registry_json()).unwrap();
        let rep = reg.entries[0]
            .inside_leg
            .as_ref()
            .expect("inside_leg stored");
        assert_eq!(rep.state, state::InsideLegState::Blocked);
        assert_eq!(rep.reason.as_deref(), Some("permission to run rm"));
        assert_eq!(
            reg.entries[0].screen_state, None,
            "capability flip must clear the scrape verdict on a blocked report too"
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

    /// A non-object `envelope` is rejected with InvalidParams BEFORE any registry
    /// or sidecar work (channel need not even exist).
    #[test]
    fn push_to_channel_rejects_non_object_envelope() {
        let home = tmp_home("push-badenv");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let resp = handle_push_to_channel(
            &ctx,
            &Request::new(
                1,
                "channel.push_to_channel",
                json!({"mcp_channel_id": "c1", "envelope": "not-an-object"}),
            ),
        );
        assert!(resp.is_err(), "non-object envelope must be InvalidParams");
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// An envelope to an unregistered channel -> ChannelUnknown; the sidecar is
    /// never invoked.
    #[test]
    fn push_to_channel_unknown_channel_errors() {
        let home = tmp_home("push-unknown");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let resp = handle_push_to_channel(
            &ctx,
            &Request::new(
                1,
                "channel.push_to_channel",
                json!({"mcp_channel_id": "nope", "envelope": {"a": 1}}),
            ),
        );
        assert!(resp.is_err(), "unknown channel must error");
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// No envelope against a registered channel -> legacy `{"routed": true}`
    /// exactly (confirm-only, unchanged; no `delivered` key).
    #[test]
    fn push_to_channel_no_envelope_is_confirm_only() {
        let home = tmp_home("push-confirm");
        seed_stream_row(&home, "worker-c", "chA");
        state::update_registry(&home.registry_json(), |r| {
            r.entries[0].mcp_channel_id = Some("c1".into());
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let resp = handle_push_to_channel(
            &ctx,
            &Request::new(
                1,
                "channel.push_to_channel",
                json!({"mcp_channel_id": "c1"}),
            ),
        );
        let result = resp.result().unwrap();
        assert_eq!(result["routed"], true);
        assert!(
            result.get("delivered").is_none(),
            "confirm-only must not claim delivery"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }
}
