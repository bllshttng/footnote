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
// The receipt builders moved to `receipt.rs` so the write choke
// point (`state::update_registry`) can stage the same recovery record for a
// row removed through ANY door; re-exported so the reap path's references
// are unchanged.
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

mod blocking_bound;
mod rm_codex_rollback;
mod rm_refusal_detail;
mod rm_teardown;
pub(crate) mod roster_death;
mod stop_refusal_detail;
pub(crate) mod store_socket_sweep;
pub(crate) mod worktree_sweep;
pub(crate) use self::blocking_bound::directory_bytes;
use self::blocking_bound::{off_executor, resolve_reclaimed_bytes};
use self::roster_death::claude_row_provably_absent;
pub(crate) use self::roster_death::{claude_row_id, pid_is_gone};
pub(crate) use self::store_socket_sweep::store_socket_sweep;
mod list_rows;
use self::list_rows::{
    activity_basis_from_truth, apply_row_contradiction, attention_sort_key, basis_word_from_truth,
    handle_list, rendered_status_from_truth,
};
pub(crate) use self::list_rows::{progress_from_truth, registry_truth_handle};
mod prune_outcome;
pub(crate) use self::prune_outcome::PruneOutcome;
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
    /// accept loop (Architecture B, plan; concurrency per).
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
    /// Fire an OS notification when a badge ENTERS `blocked`. Default
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
/// string elided, mirroring `ReconcileOutcome`'s `(name, reason)`
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
    /// which a bare `Vec<String>` of short_ids discarded.
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

pub(crate) fn is_codex_thread_entry(entry: &RegistryEntry) -> bool {
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
/// Since an unreadable registry is a startup failure, not an empty
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
        //   2. a claude shellout (`ask`/`--bg`) or adopted row. Since v9
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
    // time no longer matches, else a reused pid keeps a dead
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
        // Keyed on (short_id, pid), not short_id alone (task 1). Every
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
/// PID. `None` if the process is gone or the lookup is
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

pub(crate) use crate::gc_inventory::index_tree;
// the pane kill and its absence vocabulary moved to pane_stop.rs
// with the stop helper that now shares them.
pub(crate) use crate::pane_stop::{mux_pane_is_absent, run_mux_pane_kill};

/// Wall-clock bound for one harness removal subprocess (`run_claude_rm`). A
/// hung removal must never wedge its caller (the operator measured a 300s+
/// hang on a stuck row; the removal cannot inherit it).
pub(crate) const CASCADE_TIMEOUT: Duration = Duration::from_secs(15);

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
/// teardown arm). gemini: nothing to cascade; opencode archives in `gc_native`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CascadeOutcome {
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

pub(crate) fn run_claude_rm(short_id: &str) -> Result<(), String> {
    let dir = crate::claude_roster::removal_config_dir_for_short_id(short_id)?;
    run_claude_rm_in(dir.as_deref(), short_id)
}

/// Run `claude rm` against one account root; `None` means ambient.
pub(crate) fn run_claude_rm_in(
    config_dir: Option<&std::path::Path>,
    short_id: &str,
) -> Result<(), String> {
    let mut command = std::process::Command::new("claude");
    if let Some(dir) = config_dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = command
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
                let detail =
                    crate::truth_probe::drain_to_detail(&mut child, Duration::from_secs(2));
                // retired-ok: reports the shellout this code ran and its exit code; tells no reader to run it.
                return Err(format!("claude rm exited {code}: {detail}"));
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
pub(crate) fn cascade_codex_index(
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
    // In-process since the gate port: same answers the shelled verb
    // gave, without a subprocess per row. A probe that cannot answer
    // (probe-failed) reads None -> the caller keeps the row, fail closed.
    let v = crate::worktree_reapable::reapable(cwd);
    if v.reason == "probe-failed" {
        return None;
    }
    Some(v.reapable)
}

/// The reapable gate's answer for a removed row's worktree: the
/// verdict, plus the reason a kept tree names in its receipt.
enum WorktreeGate {
    Reapable,
    Blocked(String),
    Unanswerable(String),
}

/// Per-subprocess budget for the rm worktree path, matching the Python
/// runtime's remove bound (`subprocess.run(..., timeout=60.0)`).
const RM_SUBPROCESS_TIMEOUT_SECS: u64 = 60;

use crate::bounded_cmd::output_with_timeout;

pub(crate) use crate::worktree_reapable::{branch_merged, is_linked_worktree};

/// The gate, in-process since the port: the module runs the same
/// git probes the shelled `fno agents workspace worktree reapable` ran, so
/// the rm door keeps its answers without a subprocess per row. A `yes` then
/// meets the merge check, because this door has no sweep-style pre-filter:
/// a clean-but-unmerged branch is exactly where abandoned-but-real work
/// lives, and the contract keeps it for a human.
fn worktree_gate(cwd: &str) -> WorktreeGate {
    let v = crate::worktree_reapable::reapable(cwd);
    if v.reason == "probe-failed" {
        return WorktreeGate::Unanswerable("the reapable probe could not answer".into());
    }
    if v.reapable {
        return match branch_merged(cwd) {
            Some(true) => WorktreeGate::Reapable,
            Some(false) => WorktreeGate::Blocked("clean but the branch is not merged".into()),
            None => WorktreeGate::Unanswerable("the merged-branch probe could not answer".into()),
        };
    }
    WorktreeGate::Blocked(v.reason)
}

/// A human removed ONE named row: its worktree goes with it, through the
/// same reapable gate plus merge check the `--merged` sweep and watchdog
/// honor (dirty untouched, clean-and-unmerged never auto-pruned, clean-and-
/// merged loses the tree but keeps the branch - `git worktree remove` never
/// deletes branches). A gate that cannot answer keeps the tree; the row is
/// removed either way. `None`: the row owned no linked worktree.
fn rm_take_worktree_with(
    entry: &state::RegistryEntry,
    gate: &dyn Fn(&str) -> WorktreeGate,
    remove: &dyn Fn(&str) -> Result<(), String>,
) -> Option<PruneOutcome> {
    let cwd = entry.cwd.as_str();
    if !is_linked_worktree(cwd) {
        return None;
    }
    match gate(cwd) {
        WorktreeGate::Reapable => match remove(cwd) {
            Ok(()) => Some(PruneOutcome::Removed(cwd.to_string())),
            Err(e) => Some(PruneOutcome::Kept(format!(
                "{cwd} (git worktree remove failed: {e})"
            ))),
        },
        WorktreeGate::Blocked(reason) => Some(PruneOutcome::Kept(format!(
            "{cwd} (the gate said no: {reason})"
        ))),
        WorktreeGate::Unanswerable(why) => Some(PruneOutcome::Kept(format!("{cwd} ({why})"))),
    }
}

pub(crate) fn rm_take_worktree(entry: &state::RegistryEntry) -> Option<PruneOutcome> {
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

use crate::liveness_sweep;
/// Wall-clock epoch seconds, for GC grace math. Degrades to 0 (a pre-1970 clock
/// makes every stamped row look in-grace -> nothing reaped, the safe direction).
pub(crate) use crate::liveness_sweep::{
    apply_reconcile_change, plan_reconcile, ReconcileChange, ReconcileOutcome, SweepMode,
};
pub(crate) use crate::row_truth::{
    batched_row_probes, fold_positive_death, row_truth_handle, served_fresh_liveness,
    served_liveness_basis, title_changes,
};

pub fn now_epoch_secs() -> i64 {
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

pub(crate) fn global_events_path(home: &AgentsHome) -> PathBuf {
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

/// The shared liveness ladder as production runs it: the reader
/// extracted from `claude_resume_argv_with_truth`, now called by the reaper
/// instead of a per-caller derivation. The sessions-dir index and the truth
/// answers are both computed ONCE per closure (one sweep), however many rows
/// probe - the truth rung reads the batched map, never a per-row subprocess.
/// Injected like `store_matches` so a sweep-level test never depends on what
/// lives in the developer's real `~/.claude`.
pub(crate) fn live_liveness_prober(
    truth: std::collections::HashMap<String, String>,
    sockets: std::collections::HashMap<String, String>,
    codex_index: Option<Vec<(String, u64)>>,
) -> impl Fn(&state::RegistryEntry) -> crate::client_verbs::RowLiveness {
    move |e: &state::RegistryEntry| {
        if let Some(dead) = fold_positive_death(e) {
            return dead;
        }
        crate::client_verbs::row_liveness_with_indexed(
            e,
            &sockets,
            codex_index.as_deref(),
            |uuid: &str| truth.get(uuid).cloned(),
        )
    }
}

/// Terminal-stop sweep: `claude stop` any fire-and-forget `claude --bg`
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
                // sweep. A timeout leaves the marker for the next tick, since
                // it is retried every tick, which is the failure this feature
                // exists to prevent.
                let stopped =
                    crate::lifecycle_child::bounded_claude_stop(&short, Duration::from_secs(15))
                        .await;
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
                        // The row learns fno did this: the stamp keeps the
                        // sweep from reading the harness `stopped` state as
                        // finished work on a later tick. The write is
                        // offloaded like every other registry write on the
                        // async runtime.
                        let sweep_home = home.clone();
                        let stopped_session = marker.uuid.clone();
                        let stopped_reason = marker.reason.clone();
                        let _ = update_registry_offloaded(sweep_home.registry_json(), move |r| {
                            if let Some(entry) = r.entries.iter_mut().find(|e| {
                                e.harness_session_id.as_deref() == Some(stopped_session.as_str())
                            }) {
                                state::record_stop(entry, "terminal-sweep", Some(stopped_reason));
                            }
                        })
                        .await;
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
///. If a start time is unavailable on either side (`None` — lookup
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

// The idle-exit predicate and its drift sibling live in crate::quiet_retire
// (x-6648): the daemon file is over its line budget, so the predicates moved
// beside their tests instead of growing here.

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
///   supervisor instead of replacing the incumbent. On contention it
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

    // Record the holder in the lockfile CONTENT: pid plus start time,
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
/// reachable path (/).
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
    // here would otherwise leave a stray `.flock-probe` behind.
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

    // Startup row-count assertion (AC5), BEFORE the socket is bound: a
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

    // Architecture B (plan): ONE bounded reconcile sweep on startup,
    // as part of recovery, CONCURRENTLY with the accept loop, so a
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
        // Off the startup path and onto the blocking pool. The sweep
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
            // Test seam: hold the sweep open so a test can prove the
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
                        // Registry-side keeper sweep FIRST: re-bind
                        // surviving lane-B thread rows BEFORE the settle pass
                        // below reads them - the daemon-side twin of the pane
                        // sweep's re-adopt-before-restore ordering. Non-fatal
                        // on error: emit and serve past, like the sweep below.
                        match keeper_registry_sweep(&home_sweep, &emitter_sweep) {
                            Ok(report) => {
                                // Store-socket hygiene rides the same startup
                                // pass: dead store sockets and orphaned seat
                                // locks unlinked, live ones untouched.
                                // Non-fatal by posture.
                                let store_swept = store_socket_sweep(&home_sweep, &emitter_sweep);
                                let _ = emitter_sweep.emit(
                                    "keeper_sweep_done",
                                    &json!({
                                        "sockets": report.sockets,
                                        "rebound": report.rebound.len(),
                                        "dead": report.dead.len(),
                                        "wedged": report.wedged.len(),
                                        "superseded": report.superseded.len(),
                                        "store_unlinked": store_swept.sockets,
                                        "store_locks_unlinked": store_swept.locks,
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
                        run_reconcile_sweep(&home_sweep, &emitter_sweep, &|_| true, SweepMode::Full)
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
    // Drift signal: fingerprint the executable we are running so a
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

    // Active-backlog drain supervisor, opt-in via config.active_backlog.
    // `ab_live` keeps the daemon out of idle-exit while work is enabled;
    // `ab_shutdown` winds the task down on daemon shutdown.
    let ab_live = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ab_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sandbox = ctx.home.is_sandbox();
    let _ = ctx.emitter.emit(
        "daemon_fleet_scope",
        &json!({"scope": if sandbox { "sandbox" } else { "shared" }, "home": ctx.home.root()}),
    );
    // A sandbox home starts no supervisor: its targets resolve from the real
    // cwd and real graph, so it would work the operator's board from a
    // tempdir and pin ab_live true forever.
    let ab_handle = if sandbox {
        tokio::spawn(std::future::ready(()))
    } else {
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
    // Screen-manifest scrape gate: a slow mux stalls only its own sweep.
    let scrape_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Terminal-stop gate: a large marker set never serializes inline.
    let terminal_stop_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worktree_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Orphaned-test-binary reap gate: shells ps + a kill, off the core loop.
    let orphan_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut last_orphan_sweep = Instant::now();
    let liveness_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut last_liveness_sweep = Instant::now();
    // The periodic arms: each module owns its cadence, gate and memory.
    let machine_watch = crate::machine_watch::Arm::default();
    let merge_close = crate::merge_close::Arm::default();
    let crown_ledger = crate::king_ledger::Arm::default();
    let arm_watch = crate::arm_watch::Arm::new(ctx.opts.agents_config_cwd.clone());
    let provider_cap = crate::provider_cap_verbs::Arm::new(ctx.opts.agents_config_cwd.clone());
    // Retirement-sweep cadence: the throttle stamp beside the gate,
    // plus the next interval cell the sweep body hands back (the idle-probe
    // verdict pattern), so the tick reads a mutex instead of config files.
    let mut last_gc_sweep = Instant::now();
    let retire_interval_next = crate::gc::seed_retire_interval_cell(&ctx.opts.agents_config_cwd);
    // Dead-row GC gate: its dormant check shells out to the truth
    // probe, so it gets the same one-in-flight discipline as its neighbors.
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
    // Park sweep: same one-in-flight discipline. The verb is idempotent on a
    // store nobody touched, so an extra run is a no-op; the gate exists so a
    // slow head probe never stacks.
    let park_sweep_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let idle_probe_verdict: Arc<
        std::sync::Mutex<Option<(bool, Instant, Option<std::time::SystemTime>)>>,
    > = Arc::new(std::sync::Mutex::new(None));

    // THE RULE FOR THIS LOOP: nothing that shells out, walks the
    // registry row by row, or otherwise blocks may run INLINE in a select arm.
    // Every arm shares one thread with `accept()` and with the SIGTERM arm, so
    // an inline sweep makes the daemon both unreachable and unstoppable at the
    // same time -- which is how 59 supervisors accumulated, each new client
    // reading the silence as "no daemon" and starting another. Give a sweep
    // `spawn_blocking` plus a one-in-flight `AtomicBool`, like the four below.
    // The reason the serve loop ended, threaded to the shared exit tail so
    // `daemon_exited` can tell an abnormal ending from a graceful one
    // every break carries its reason string.
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
                // Bound-inode self-check: if the socket path no
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
                // Retirement sweep: a row leaves when its work is done
                // (reverse join) and its transcript is quiet past
                // `agents.retire_grace_s`; held process stopped first, receipt
                // written before the drop, clean worktree pruned. Throttled to
                // `agents.retire_interval_s`; off-loop.
                let retire_interval = crate::gc::retire_interval_snapshot(&retire_interval_next);
                crate::gc::maybe_retirement_sweep(
                    &mut last_gc_sweep,
                    &gc_in_flight,
                    &retire_interval_next,
                    ctx.home.clone(),
                    ctx.opts.agents_config_cwd.clone(),
                    ctx.home.events_jsonl(),
                    retire_interval,
                    || crate::gc::mux_tab_sweep(false, false),
                    crate::gc::production_roster_sweep,
                );
                // Worktree sweep + merge reaper: the sweep backstops
                // what the reaper cannot reach; the reaper is the merge-triggered
                // consumer of `merge_cleanup_requested` (60s floor). Both
                // off-loop; grace and stop order live in merge_reap.rs.
                if !worktree_sweep_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&worktree_sweep_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    let grace_cwd = ctx.opts.agents_config_cwd.clone();
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        let roots = worktree_sweep::registry_repo_roots(&home);
                        let now = now_epoch_secs();
                        worktree_sweep::worktree_sweep(&home, &emitter, now, &roots, &|root| {
                            // A pending merge-cleanup request is the standing
                            // order: the pass applies while one waits.
                            crate::merge_reap::merge_cleanup_requested(&home, root).into()
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
                                Ok(output) => worktree_sweep::WorktreeSweepOutput {
                                    exit_code: output.status.code(),
                                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                                },
                                Err(error) => worktree_sweep::WorktreeSweepOutput {
                                    exit_code: None,
                                    stdout: String::new(),
                                    stderr: error.to_string(),
                                },
                            }
                        });
                        let grace_secs =
                            crate::agents_config::retire_grace_secs(&grace_cwd) as i64;
                        crate::merge_reap::consume_merge_cleanup_requests(
                            &home, &roots, &emitter, grace_secs,
                        );
                        // Daily janitor; gate and receipt in reclaim.rs.
                        crate::reclaim::maybe_run_daily(&home);
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
                // The machine gets an arm: bands the box, escalates, gates nothing.
                crate::machine_watch::maybe_tick(&machine_watch, ctx.home.clone());
                crate::merge_close::maybe_tick(&merge_close, ctx.home.clone());
                // reign.html renders on a beat even with no crown live.
                crate::king_ledger::maybe_tick(&crown_ledger, ctx.home.clone());
                crate::arm_watch::maybe_tick(&arm_watch, ctx.home.clone());
                crate::provider_cap_verbs::maybe_tick(&provider_cap, ctx.home.clone());
                // Serve-only liveness tick: the served pair is the sweep's measurement,
                // refreshed every SERVED_LIVENESS_CADENCE; off-loop, one-in-flight.
                let codex_threads_for_liveness = Arc::clone(&ctx.codex_threads);
                crate::liveness_sweep::maybe_sweep(
                    &mut last_liveness_sweep,
                    &liveness_sweep_in_flight,
                    ctx.home.clone(),
                    ctx.home.events_jsonl(),
                    Arc::new(move |entry: &RegistryEntry| {
                        match codex_threads_for_liveness.try_lock() {
                            Ok(guard) => guard.contains_key(&entry.name),
                            // An actor is mid insert/remove: hosted, so a race
                            // can never settle a thread the map is about to name.
                            Err(_) => true,
                        }
                    }),
                );
                // Terminal-stop sweep: exit fire-and-forget `claude --bg`
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
                // Stale-question reconcile, the arm above `stale_sweep`: its
                // doc comment there covers the shape.
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
                // Park sweep, the arm beside `stale_sweep`: same doc comment
                // there covers the one-in-flight shape. The run closure walks
                // every repo root the registry knows, so parked rows of other
                // repos are un-parked from THEIR checkout (the head probe resolves PR numbers against the repo).
                if !park_sweep_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&park_sweep_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        park_sweep(&home, &emitter, now_epoch_secs(), &|| {
                            sweeps::sweep_all_roots(&home)
                        });
                    });
                }
                crate::question_sweep::daemon_tick(&ctx.home, now_epoch_secs());
                // An enabled active-backlog project keeps the daemon resident
                // (OQ1 Option A): idle-exit must never kill a live supervisor.
                let ab_active = ab_live.load(std::sync::atomic::Ordering::SeqCst);
                // Drift retirement (x-6648): the on-disk binary changing under
                // a running daemon is a retirement request at the same quiet
                // boundary idle-exit owns -- same fresh no-worker probe, same
                // graceful tail, distinct receipt. A daemon with live work
                // keeps serving the old build until a quiet tick settles.
                let drift = ctx.exe_fingerprint.as_ref().map(crate::drift::self_drift);
                let retire_reason = crate::quiet_retire::quiet_retire_reason(
                    drift.as_ref(),
                    ab_active,
                    last_activity.elapsed() >= ctx.opts.idle_exit,
                );
                if retire_reason.is_some() {
                    // The liveness read (blocking CONNECT probes) runs OFF the
                    // select arm: an in-arm probe against a wedged worker's
                    // filling backlog is the unreachable-AND-unstoppable shape
                    // this loop's rule exists to prevent. One probe in flight;
                    // exit fires on its verdict: worst case one extra 5s tick.
                    if !idle_probe_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        let flag = Arc::clone(&idle_probe_in_flight);
                        let home = ctx.home.clone();
                        let verdict = Arc::clone(&idle_probe_verdict);
                        let probe_activity = last_activity;
                        tokio::task::spawn_blocking(move || {
                            let _gate = SweepGate(flag);
                            let no_worker = crate::quiet_retire::no_live_worker(&home);
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
                        crate::quiet_retire::probe_verdict_fresh(
                            no_worker,
                            probe_activity,
                            last_activity,
                            probe_mtime,
                            std::fs::metadata(ctx.home.registry_json())
                                .ok()
                                .and_then(|m| m.modified().ok()),
                        )
                    });
                    if fresh {
                        if let Some(crate::drift::DriftState::Drifted { running, on_disk }) =
                            &drift
                        {
                            let _ = ctx.emitter.emit(
                                "daemon_drift_pending_exit",
                                &json!({
                                    "running": running.path.display().to_string(),
                                    "running_size": running.size,
                                    "on_disk": on_disk.path.display().to_string(),
                                    "on_disk_size": on_disk.size,
                                }),
                            );
                        }
                        emit_state(&ctx.emitter, DaemonState::IdlePendingExit);
                        let _ = ctx.emitter.emit("daemon_idle_pending_exit", &json!({}));
                        emit_state(&ctx.emitter, DaemonState::ShuttingDown);
                        let _ = ctx.emitter.emit(
                            "daemon_shutting_down",
                            &json!({"reason": retire_reason}),
                        );
                        break retire_reason.unwrap();
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
    // successor's socket, the same discipline stop_worker_confirmed
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
    let _ = ctx.emitter.emit(
        "daemon_exited",
        &crate::quiet_retire::daemon_exited_payload(exit_reason),
    );
    Ok(())
}

/// Daemon-wide context passed to handlers.
struct Ctx {
    home: AgentsHome,
    emitter: EventEmitter,
    opts: DaemonOptions,
    started_at: Instant,
    /// Fingerprint of the executable this daemon is running,
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

/// One actor task per thread owns its daemon connection exclusively.
/// This used to be `Arc<tokio::sync::Mutex<CodexThread>>`, which baked
/// whole-turn exclusion into the HANDLE TYPE: `drive_turn` held the guard for
/// up to `TURN_TIMEOUT` (600s), so every follow-up ask blocked behind the
/// active turn, the steer RPC was unreachable, the detached seed task held the
/// same lock, and `stop` removed a handle whose turn task still owned a clone
/// while stamping `Exited`. Consumers now send [`ThreadCommand`]s and never
/// touch the driver; see `crates/fno-agents/src/codex_thread.rs`.
type CodexThreadHandle = Arc<crate::codex_thread::CodexThreadActor>;

use crate::codex_thread::InterruptOutcome;

mod codex_thread_lane;
mod codex_thread_resume;
mod thread_row_status;
use codex_thread_lane::spawn_codex_thread_lane;
use codex_thread_resume::{ensure_codex_thread_handle, schedule_codex_thread_recovery};
pub(crate) use thread_row_status::notify_transition;
use thread_row_status::{
    codex_thread_on_done, codex_thread_on_status, gate_inside_leg_onto_row, notify_badge,
};

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

/// The daemon-face read: the typed decode plus the raw-count
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
///. Mirrors the `update_registry_offloaded` wrapper and the
/// `run_blocking` helper. Since a join failure maps to a `StateError`
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

/// The RPC face of a registry-read failure: every handler that consults
/// the registered roster reports `registry read failed` carrying the state
/// error (which names the registry path, both row counts, and the comparison
/// to run) instead of answering from a silently emptied roster. `AgentNotFound`
/// stays reserved for a successful read with no matching row.
fn registry_read_failed(id: u64, e: state::StateError) -> Response {
    let code = state_error_code(&e);
    Response::err(id, code, format!("registry read failed: {e}"))
}

/// Offload the blocking read-modify-write of `state::update_registry` to the
/// blocking pool. The closure runs on the blocking thread, so it
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
pub(crate) fn is_non_terminal(s: AgentStatus) -> bool {
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
    // Post-G4: the daemon hosts no agent PTYs, so the only spawns it
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
    // The door pair arrives on the request from the client, which
    // proved the caller at the earliest boundary. A malformed pair refuses
    // before any worker effect; an absent pair keeps the request-edge mint.
    let provenance = match crate::spawn_contract::parse_request_provenance(p) {
        Ok(v) => v,
        Err(reason) => {
            return Response::err(req.id, ErrorCode::InvalidParams, reason);
        }
    };
    if substrate == "thread" {
        return route_thread_spawn(ctx, req, &name, &cwd, &provider, provenance.as_ref()).await;
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
                provenance.as_ref(),
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
        "daemon PTY hosting was retired at G4 : spawn a mux-hosted agent pane with \
         `fno agents spawn --substrate pane`, or use `--substrate bg|headless`. The daemon \
         serves only claude stream-json adoption (host_mode=interactive, mode=stream_json).",
    )
}

/// Route a thread-substrate spawn from the capability contract alone, never
/// the harness name: `thread_lane` picks the lane, and on the attach lane
/// `attach_needs_server` splits harness-owned-server attaches (the app-server
/// lane below) from harness-owned-client ones, which this daemon cannot serve.
/// An unreadable table or unknown harness refuses rather than routing. Every
/// arm but attach-with-server refuses, so the invariant holds.
async fn route_thread_spawn(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    cwd: &Path,
    provider: &str,
    provenance: Option<&crate::spawn_contract::SpawnProvenance>,
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
            Ok(true) => spawn_codex_thread_lane(ctx, req, name, cwd, provider, provenance).await,
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
// Claude stream-json host lane front door (Group 3).
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
/// The agent-list row's substitution marker: the object naming BOTH
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
    provenance: Option<&crate::spawn_contract::SpawnProvenance>,
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
    //    fatal to the spawn: an empty-roster default here would read
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
    let entry = crate::claude_stream_entry::build_claude_stream_entry(
        name,
        &short_id,
        cwd,
        uuid,
        worker_pid,
        worker_pid_start_time,
        ctx.home.timeline_jsonl(&short_id),
        req.params.get("node").and_then(Value::as_str),
        &req.params,
        provenance,
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
    let birth = crate::spawn_edge::birth_event(
        name,
        &crate::state::Lineage::from_request(&req.params),
        json!({"provider": "claude", "short_id": short_id, "lane": "stream", "session_uuid": uuid, "node": req.params.get("node").and_then(Value::as_str)}),
    );
    let _ = ctx.emitter.emit("agent_spawned", &birth);

    Response::ok(
        req.id,
        json!({"short_id": short_id, "harness": "claude", "status": "live", "lane": "stream"}),
    )
}

/// The turn a seedless codex thread spawn takes so a rollout exists
/// and the worker is attachable immediately. Deliberately trivial: it must
/// cost one small turn and leave a transcript line an operator reads as
/// startup rather than as work someone asked for.
const WARMUP_SEED: &str = "Reply with the single word: ready.";

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

    // a blind read must not claim a live recipient is absent. The
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

    // A codex hosted thread is driven through its actor: submit the
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
    F: Fn(
        &[String],
    ) -> (
        std::collections::HashMap<String, crate::truth_probe::TruthProbe>,
        crate::truth_probe::BatchOutcome,
    ),
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
            "writing" | "quiet" | "parked" | "orphaned" | "refused" | "unknown"
        ) {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!(
                    "invalid --status '{st}' (status is served activity: writing | quiet | parked | orphaned | refused | unknown; process liveness is the liveness field on fno agents list --json)"
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

    // an unreadable registry is an RPC error, never a valid empty
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
    let (truths, batch_outcome) = truth_fn(&handles);
    // The instrument's own receipt: a page where the probe answered
    // nothing must be readable AS that, not as 43 rows confidently `unknown`.
    // The rendered status word cannot carry the distinction (the vocabulary is
    // frozen by the --status filter), so the envelope does.
    let truth_probe_asked = handles.len();
    let truth_probe_answered = handles.iter().filter(|h| truths.contains_key(*h)).count();
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
            // absent, never as no-evidence. Both basis legs are worded, never
            // blank-on-a-guess: `basis_word_from_truth` decides between an
            // absent reading and a page the batch never measured, and
            // `activity_basis_from_truth` names the age's instrument beside
            // it.
            let basis_word = basis_word_from_truth(truth.as_ref(), batch_outcome);
            let activity_basis = activity_basis_from_truth(truth.as_ref(), batch_outcome);
            let evidence = (
                json!(truth.as_ref().and_then(|t| t.reachability.as_deref())),
                basis_word,
                json!(truth.as_ref().and_then(|t| t.last_activity_age_s)),
                json!(truth.as_ref().and_then(|t| t.last_event_at.as_deref())),
                json!(truth.as_ref().and_then(|t| t.last_message.as_deref())),
                activity_basis,
            );
            // The orthogonal axis: reachability answers "can I reach this
            // process"; progress answers "is it advancing, awaiting the
            // operator, parked, or refused" -- read off the SAME probe, so a
            // refused-but-reachable row is never rendered as a fourth
            // reachability value.
            let (progress, progress_basis) = progress_from_truth(
                truth.as_ref(),
                batch_outcome,
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
                let (
                    reachability,
                    basis,
                    last_activity_age_s,
                    last_event_at,
                    last_message,
                    last_activity_basis,
                ) = evidence;
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
                    // until, which hid the real vendor axis from every
                    // RPC consumer. `observed_model` below remains the honest
                    // answer to what actually answered.
                    "harness": e.harness_name(),
                    "provider": e.provider,
                    // Stored effort is a separate spawn axis. It is passed
                    // through unchanged; observed_model remains transcript truth.
                    "effort": e.effort,
                    "harness_session_id": e.harness_session_id,
                    // The two identity axes plus classified lineage,
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
                    // The SERVED liveness triple: word, stamp, basis; the freshness
                    // rule is served_liveness.rs, mirrored per crate, pinned by contract test.
                    "liveness": served_fresh_liveness(
                        e.liveness.as_deref(),
                        e.liveness_measured_at.as_deref(),
                    ),
                    "liveness_basis": served_liveness_basis(
                        e.liveness.as_deref(),
                        e.liveness_measured_at.as_deref(),
                    ),
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
                    // The instrument the age came from (`last-entry` |
                    // `mtime` | `opencode-db`), the resolver's reason word
                    // (`not-found` | `no-records` | `resolver-error`) when it
                    // could not resolve the handle, or `unmeasured` when the
                    // batch never ran for this page - never a bare null: the
                    // three unknown-reason words are the difference between
                    // "no transcript" and "the resolver crashed".
                    "last_activity_basis": last_activity_basis,
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
                    // v23: the stored REQUEST beside the observation,
                    // plus the substitution marker derived from the payload
                    // above - the same two keys, computed the same way, as
                    // Python's serialize_entry. Null marker is match-or-
                    // unknown; it never reads as a clean bill on its own.
                    "requested_model": e.requested_model,
                    "model_substituted": model_substitution_marker(
                        e.requested_model.as_deref(),
                        &observed_model,
                    ),
                    // Architecture C (plan): additive keys, never removing
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
                    // The lane the row was spawned on, read from the
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
                    // The parent edge the orphan check keys on; null is a real answer.
                    "spawned_by_session": e.spawned_by_session,
                    // The served CHILD/PEER word; null before the first stamp.
                    "lineage_kind": e.lineage_kind,
                    // How this session came to exist: "operator" for one a human
                    // started by hand, "spawn" for a footnote-created worker, null
                    // for a row nothing stamped. Emitted on BOTH serializers because
                    // `fno agents list` auto-routes to this projection whenever an
                    // installed binary is present, so a Python-only key would be
                    // missing from the path nearly every reader takes.
                    "origin": e.origin,
                    // the row's mail delivery policy ("bus-only" holds
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
                    apply_row_contradiction(object, e.exited_at.as_deref(), chrono::Utc::now());
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
            "truth_probe_asked": truth_probe_asked,
            "truth_probe_answered": truth_probe_answered,
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
    // failure is an RPC error: `unwrap_or_default()` here published
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
                // Drift signal, additive. Null when the daemon
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
    let mut response = stop_body(ctx, req).await;
    attach_stopped_claims_release(ctx, req, "stop", &mut response).await;
    response
}

async fn stop_body(ctx: &Ctx, req: &Request) -> Response {
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
    // A pane-hosted row's ONE live ref is the mux pane; the refusal text and
    // its branch table live in stop_refusal_detail. Above the claude branch on
    // purpose: stop_claude's no-transport-id fallback signals the recorded pid
    // instead, which kills the process inside the pane and leaves the pane
    // itself.
    if let Some(mux) = entry.mux.as_ref() {
        let (probe, precheck) = stop_refusal_detail::pane_verdict(&entry);
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            stop_refusal_detail::pane_row_refusal(
                &name,
                &mux.session,
                mux.pane_id,
                probe,
                precheck.as_ref(),
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
        let interrupt_report = match rm_teardown::end_codex_thread(ctx, &name).await {
            Ok(report) => report,
            // Keep the handle and leave the row non-terminal. The actor still
            // holds the interrupt handle for the live turn, so a retry can
            // reach it, and a terminal row would also make the thread
            // invisible to `codex_thread_recovery_candidate`.
            Err(interrupt_report) => {
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
        };
        let stop_name = name.clone();
        if let Err(error) = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
            if let Some(entry) = registry.find_mut(&stop_name) {
                entry.status = AgentStatus::Exited;
                entry.exited_at = Some(now_rfc3339_like());
                crate::state::record_stop(entry, "stop-verb", Some("codex-thread".into()));
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
    // A lane-B keeper thread: fno's own keeper hosts the child and
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
                crate::state::record_stop(entry, "stop-verb", Some("keeper-thread".into()));
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
            crate::state::record_stop(e, "stop-verb", None);
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

/// One release per confirmed stop/rm verb (change 5). Resolves the
/// stopped row's identity, scans the global claims dir and the row's own
/// space claims dir, releases the provably-dead claims the stopped holder
/// keeps, emits ONE audit event, and rides the receipt on the response under
/// `claims`. A `stopped: false` response (a codex interrupt that did not
/// settle) releases nothing and carries no `claims` key.
async fn attach_stopped_claims_release(
    ctx: &Ctx,
    req: &Request,
    verb: &str,
    response: &mut Response,
) {
    let confirmed = match response.result() {
        Some(result) => {
            // A no-op stop (a synchronous provider with nothing to stop)
            // reports stopped:true but stopped nothing, so it releases
            // nothing - the same contract the Python no-op arm holds.
            !matches!(result.get("no_op"), Some(Value::Bool(true)))
                && (matches!(result.get("stopped"), Some(Value::Bool(true)))
                    || matches!(result.get("already_exited"), Some(Value::Bool(true)))
                    || matches!(result.get("removed"), Some(Value::Bool(true))))
        }
        None => false,
    };
    if !confirmed {
        return;
    }
    let Some(name) = req.params.get("name").and_then(Value::as_str) else {
        return;
    };
    // A stop leaves the row (terminal); an rm removes it, so the caller
    // passes the resolved identity instead of re-reading the registry.
    let identity = load_registry_offloaded(ctx.home.registry_json())
        .await
        .ok()
        .and_then(|registry| {
            registry
                .entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| (e.harness_session_id.clone(), Some(e.cwd.clone())))
        });
    let (session_id, cwd) = identity.unwrap_or((None, None));
    release_stopped_claims_into(&ctx.emitter, name, session_id, cwd, verb, response);
}

/// The synchronous core of the release: resolve dirs, run
/// [`crate::claims::release_for_stopped_session`], emit the event, insert the
/// receipt under `claims`. Shared by the stop wrapper and handle_rm_with's
/// tail (where the row is already gone from the registry).
fn release_stopped_claims_into(
    emitter: &EventEmitter,
    name: &str,
    session_id: Option<String>,
    cwd: Option<String>,
    verb: &str,
    response: &mut Response,
) {
    let Some(result) = response.result_mut().and_then(|r| r.as_object_mut()) else {
        return;
    };
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(global) = crate::claims::global_claims_dir() {
        dirs.push(global);
    }
    if let Some(space_claims) = cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(|cwd| crate::paths::space_dir_opt(std::path::Path::new(cwd)))
        .map(|dir| dir.join("claims"))
        .filter(|dir| !dirs.contains(dir))
    {
        dirs.push(space_claims);
    }
    if dirs.is_empty() {
        return;
    }
    let target = crate::claims::StoppedHolder {
        name: name.to_string(),
        harness_session_id: session_id.clone(),
    };
    let receipt = match crate::claims::release_for_stopped_session(&target, &dirs, None) {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = emitter.emit(
                "agent_stop_claims_released",
                &json!({"name": name, "verb": verb, "error": error}),
            );
            return;
        }
    };
    let _ = emitter.emit(
        "agent_stop_claims_released",
        &json!({
            "name": name,
            "session_id": session_id.clone(),
            "verb": verb,
            "released": receipt.released,
            "kept": receipt.kept,
        }),
    );
    result.insert(
        "claims".into(),
        serde_json::to_value(&receipt).unwrap_or(Value::Null),
    );
}

/// Bound on a worker's shutdown ACK (review): a wedged worker must not
/// hang the daemon's stop handler - the client above it would then report the
/// DAEMON as unresponsive and prescribe killing it, orphaning the very worker
/// being stopped. No ack inside this window reads as no ack; the caller's
/// SIGTERM -> SIGKILL escalation is the recovery.
const WORKER_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the WRITE half of a `worker.shutdown` round trip (self-review
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
/// retirement sweep, which holds an `AgentsHome` and no `Ctx`.
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
    // if the recorded pid is alive but its start time no longer
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
/// still running. Only ESRCH is death - except the zombie, which is dead but
/// not yet reaped: `kill(pid, 0)` keeps succeeding while it holds no fds and
/// serves nothing, so `census::pid_is_zombie` decides that arm.
fn pid_confirmed_dead(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        // Never signalled in the first place, so nothing is running on our behalf.
        return true;
    }
    // SAFETY: signal 0 is an existence/permission probe only, no signal is sent.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        // Reachable => alive, unless it is a zombie: dead-but-unreaped.
        return crate::census::pid_is_zombie(pid);
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Whether `pid` is PROVABLY a different incarnation than the one recorded:
/// reachable, readable start token, and different. Not the negation of
/// `pid_is_ours` (whose false covers dead, recycled, AND unsignalable) -- less
/// positive evidence than this leaves the caller waiting and reporting failure.
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

/// Poll until `pid` is confirmed dead or `budget` elapses. A RECYCLED pid also
/// ends the wait; an unsignalable-but-alive process runs the clock out (failure
/// is the honest answer when we cannot see).
pub(crate) async fn pid_gone_within(
    pid: u32,
    recorded_start: Option<u64>,
    budget: Duration,
) -> bool {
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
                     stopping has no exit here: the row can neither prove liveness \
                     nor be addressed. The override for that case is documented in \
                     `fno agents rm --help`, not here."
                ),
            );
        }
    };
    // Bound the subprocess so a hung `claude` can never wedge this RPC
    // handler, the same way the background-sweep twin above is bounded.
    match crate::lifecycle_child::bounded_claude_stop(&short, Duration::from_secs(15)).await {
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
            // contract for exactly the rows makes readable (Codex P2).
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

/// What a read-only look at the pane referent proved. `Unknown` is the
/// fail-closed posture: a probe that cannot prove absence changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneProbe {
    Present,
    Absent,
    Unknown,
}

/// Probe whether the pane a registry row's mux ref names still exists, without
/// touching it: a one-line `pane read` against that session. The absence
/// vocabulary is the same `mux_pane_is_absent` set the kill cascade trusts, so
/// "absent" means the mux layer itself said the pane is gone.
pub(crate) fn run_mux_pane_probe(session: &str, pane_id: u64) -> PaneProbe {
    let pane = pane_id.to_string();
    let mut child = match std::process::Command::new("fno")
        .args([
            "mux", "pane", "read", "--server", session, "--lines", "1", &pane,
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
        &crate::claude_roster::read_all_agents_union,
        &run_claude_rm,
        &rm_teardown::claude_stop_confirmed,
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
    let Some(kings) = crate::paths::space_dir_opt(std::path::Path::new(&entry.cwd)) else {
        return;
    };
    let path = kings.join("kings").join(format!("{scope}.md"));
    // Owner guard, the Rust half of Python remove_king_manifest's
    // expected_harness_session_id: a successor crowned over this scope after
    // the row went terminal can have re-armed the manifest with ITS session
    // id, and deleting unconditionally would disarm that live king. Skip only
    // on a PROVEN foreign owner (the manifest names a different session id);
    // an id-less or matching manifest deletes on the registry's own authority,
    // which is what rm acts on.
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
    claude_stop: &(dyn Fn(&str) -> bool + Sync),
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
    let mut claude_agents = if entry.harness_name() == "claude" {
        Some(off_executor(read_claude_agents))
    } else {
        None
    };
    // The stored enum is what fno last WROTE, not what is true: a session torn
    // down by hand never updates it. Two truths prove the row gone, each with
    // its own fail-closed posture. A claude row absent from the `claude agents
    // --json --all` roster is provably gone, whoever removed it (claude-only;
    // `claude_row_provably_absent` is unconditionally false elsewhere). A pane
    // row whose pane the probe cannot find is provably gone - the pane is that
    // row's ONE live ref - and a claude row whose roster state is terminal or
    // whose roster pid is provably gone is finished even though Claude keeps
    // it listed. Anything less keeps refusing, and `--force` remains the only
    // escape. One death verdict for the whole gate, shared with the reaper: a
    // merge cleanup whose stop cleared on it must not be refused by the very
    // next `fno agents rm`.
    let mut provably_gone = claude_agents
        .as_ref()
        .is_some_and(|snapshot| crate::gc_sweep::claude_death_reason(&entry, snapshot).is_some())
        || claude_row_provably_absent(claude_agents.as_ref(), harness_row_id.as_deref())
        || off_executor(|| pane_provably_absent(entry.mux.as_ref(), mux_pane_probe));
    // Law d-81c6da7e: remove needs no prior stop. rm owns the one exception's
    // stop - the claude background thread - itself: run the bounded `claude
    // stop`, then re-read the roster and recompute the death verdict. Every
    // other live row is ended by the arms below; no caller composes a stop
    // leg in front of this verb anymore.
    if entry.status == AgentStatus::Live
        && !force
        && !provably_gone
        && crate::gc_native::stop_precedes_removal(&entry)
    {
        let short = entry.transport_short().map(str::to_string).or_else(|| {
            entry
                .harness_session_id
                .clone()
                .filter(|session_id| !session_id.trim().is_empty())
        });
        if let Some(short) = short {
            let _ = off_executor(|| claude_stop(&short));
            let snapshot = off_executor(read_claude_agents);
            provably_gone = crate::gc_sweep::claude_death_reason(&entry, &snapshot).is_some()
                || claude_row_provably_absent(Some(&snapshot), harness_row_id.as_deref());
            claude_agents = Some(snapshot);
        }
    }
    // Only a claude background thread can still be refused here: every other
    // live row falls through to the arms that end its process.
    if entry.status == AgentStatus::Live
        && !force
        && !provably_gone
        && crate::gc_native::stop_precedes_removal(&entry)
    {
        let row = harness_row_id
            .clone()
            .unwrap_or_else(|| "(no harness row id)".into());
        let roster_known = claude_agents.as_ref().is_some_and(|snap| snap.is_known());
        let warnings = claude_agents
            .as_ref()
            .map(|snapshot| snapshot.warning_text())
            .unwrap_or_default();
        let row_present = harness_row_id.as_deref().is_some_and(|id| {
            claude_agents
                .as_ref()
                .is_some_and(|snap| snap.find(id).is_some())
        });
        let detail = rm_refusal_detail::live_row_refusal(
            &name,
            &row,
            harness_row_id.is_none(),
            roster_known,
            row_present,
            &warnings,
        );
        return Response::err(req.id, ErrorCode::Busy, detail);
    }
    // rm owns the codex thread's process end (law d-81c6da7e): the interrupt,
    // settle and actor drop happen HERE, before the harness cascade. A turn
    // that does not settle leaves the row and the codex index entry
    // untouched.
    if is_codex_thread_entry(&entry) {
        if let Err(interrupt_report) = rm_teardown::end_codex_thread(ctx, &name).await {
            return Response::err(
                req.id,
                ErrorCode::Busy,
                format!(
                    "agent {name}: the codex thread's turn did not settle \
                     ({interrupt_report}); the registry row and the codex index \
                     entry are kept"
                ),
            );
        }
    }
    let codex_index_capture = rm_codex_rollback::CodexIndexCapture::before_cascade(&entry);
    let harness_outcome = off_executor(|| {
        crate::gc_native::cascade_harness_session_result_with(
            &entry,
            claude_agents.as_ref(),
            read_claude_agents,
            claude_rm,
        )
    });
    if let CascadeOutcome::Failed(reason) = &harness_outcome {
        if !force {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!("agent {name}: harness removal failed: {reason}"),
            );
        }
    }
    let pane_outcome;
    let mut pane_stop_detail: Option<String> = None;
    let pane_arm_ran = entry.substrate.as_deref() == Some("pane");
    if pane_arm_ran {
        // a pane row's ONE live ref is the pane process, and a
        // successful or absent pane kill is not a death - the stored pane id
        // is not an address for a process a keeper re-adopt. rm proves the
        // stop the same way the reap does: verified pid, pane found by child
        // pid, ESRCH only.
        let e_for_stop = entry.clone();
        let stop = off_executor(move || crate::pane_stop::stop_pane_process_confirmed(&e_for_stop));
        let (outcome, detail) = crate::pane_stop::rm_pane_outcome(&stop);
        pane_outcome = outcome;
        pane_stop_detail = detail;
    } else if let Some(mux) = entry.mux.as_ref() {
        pane_outcome = match off_executor(|| mux_pane_kill(&mux.session, mux.pane_id)) {
            Ok(true) => CascadeOutcome::Removed,
            Ok(false) => CascadeOutcome::AlreadyAbsent("mux pane already absent".into()),
            Err(reason) => CascadeOutcome::Failed(reason),
        };
    } else {
        pane_outcome = CascadeOutcome::NotApplicable;
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
            if pane_arm_ran {
                // The pane arm's reason IS the stop measurement; there may be
                // no mux ref at all to name.
                return Response::err(
                    req.id,
                    ErrorCode::Internal,
                    format!(
                        "agent {name}: {harness_note}registry retained; the pane stop did not confirm: {reason}"
                    ),
                );
            }
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
        && (force || provably_gone || !crate::gc_native::stop_precedes_removal(&entry))
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
            codex_index_capture.restore_on_registry_failure();
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
    // The row is gone from the registry: take its worktree, but only
    // as far as the reapable gate allows. The receipt rides the RESULT (the
    // operator's notice), deliberately NOT the event: agent_removed sits
    // near the 500-byte event cap already, so the receipt would push every
    // rm event over it and the writer would replace the whole record. The
    // auditable event field is to land with a shape that fits.
    let worktree_path = std::path::Path::new(&entry.cwd);
    let detected_worktree = is_linked_worktree(&entry.cwd);
    let worktree_touched = audit.worktree_touched.unwrap_or(detected_worktree);
    // Measured and taken in ONE off-executor hop: they are adjacent, both
    // filesystem-bound, and together they are the longest blocking stretch in
    // the handler - and it runs after the row is already gone.
    let (measured_bytes, worktree_receipt) = off_executor(|| {
        let measured = if detected_worktree {
            directory_bytes(worktree_path)
        } else {
            None
        };
        (measured, rm_take_worktree(&entry).map(|o| o.receipt()))
    });
    let worktree_removed = worktree_touched && !worktree_path.exists();
    let reclaimed_bytes =
        resolve_reclaimed_bytes(audit.reclaimed_bytes, worktree_removed, measured_bytes);
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
        Ok(()) if event_payload_len > crate::events_limits::max_data_bytes() => Some(format!(
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
        // a confirmed pane stop's detail (pane killed, pid gone)
        // rides here because `Removed` carries no reason of its own.
        "pane_reason": pane_stop_detail.as_deref().or(pane_outcome.reason()),
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
    let mut response = Response::ok(req.id, result);
    release_stopped_claims_into(
        &ctx.emitter,
        &name,
        entry.harness_session_id.clone(),
        Some(entry.cwd.clone()),
        "rm",
        &mut response,
    );
    response
}

/// `reachability` per-call timeout (LD30): a single provider probe is bounded.
const RECONCILE_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// Total reconcile sweep budget (LD30): beyond it, remaining agents defer to the
/// next tick so a large registry never blocks the daemon for long.
const RECONCILE_SWEEP_BUDGET: Duration = Duration::from_secs(5);

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
        // Python writes opencode ids to the canonical harness_session_id and drops
        // `session_id` on write; falling through to it would hand the probe None
        // for every pane row and make it a permanent no-op.
        "opencode" => e
            .harness_session_id
            .clone()
            .or_else(|| e.session_id.clone()),
        _ => e.session_id.clone(),
    };
    crate::provider::AgentEntry {
        name: e.name.clone(),
        provider: e.harness_name().to_string(),
        substrate: e.substrate.clone(),
        session_id,
        cwd: PathBuf::from(&e.cwd),
    }
}

/// Everything the `reconcile` RPC needs to render its response, returned by
/// [`run_reconcile_sweep`] so the bounded sweep core is shared with the daemon's
/// startup pass (Architecture B, plan).
pub(crate) struct ReconcileSweepResult {
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
/// Late bind (task 2): resolve a pane-hosted codex row's session id on
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
// Registry-side keeper sweep.
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
/// start: the registry-side consumer of the keeper discovery, keyed
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

pub(crate) fn run_reconcile_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    thread_hosted: &dyn Fn(&RegistryEntry) -> bool,
    mode: SweepMode,
) -> Result<ReconcileSweepResult, String> {
    use crate::provider::ReachabilityProbeError;

    // Late bind (task 2), before the registry snapshot below is taken,
    // so a row bound this tick is already visible to the probe/reconcile pass
    // that follows. Serve-only skips it: the tick re-measures, it does not
    // re-bind identities.
    if matches!(mode, SweepMode::Full) {
        late_bind_codex_sessions(home, emitter, &codex_session_for_pid_shellout)?;
    }

    // a broken registry is a failed sweep, never a successful zero-row
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
    // The zombie flip fires only when the roster read SUCCEEDED: an
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
    // The shared reads are built HERE, before the clock: the socket index
    // and the codex rollout index serve every probed row, and their lazy
    // first build was charged to the sweep budget (measured: the sessions
    // walk alone exceeded the whole 5s window, so every later row deferred).
    let prober = live_liveness_prober(
        truth,
        crate::client_verbs::sessions_socket_index(&crate::claude_ask::ClaudeHome::from_env()),
        crate::client_verbs::codex_rollout_index(None),
    );
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
    // Gated on the same predicate the applier uses, so a ServeOnly tick that
    // writes a pid-proven exit also publishes its completion.
    for ch in &changes {
        if liveness_sweep::mode_writes_status(&mode, ch)
            && matches!(ch.new_status, Some(AgentStatus::Exited))
        {
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
        liveness_sweep::apply_reconcile_changes(r, &entries, &changes, &titles, &mode, &now);
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

    // Roster-progress refresh (SECOND HALF): the same per-tick set the
    // reconcile sweep just probed - but this loop's own git/gh subprocess
    // calls are NOT covered by the probe loop's budget check above (that one
    // stops feeding `plan_reconcile` new entries; it does not bound what runs
    // after). Re-check the SAME `start`/`RECONCILE_SWEEP_BUDGET` clock here so
    // a large changed-row set cannot extend a sweep that runs synchronously at
    // daemon startup and blocks `accept()` on every `reconcile` RPC. Remaining
    // rows are simply deferred to the next tick, the same fairness the probe
    // loop itself relies on. Best-effort and non-fatal otherwise: an I/O
    // failure here must never fail the sweep that already wrote the registry.
    // Full-only: the serve-only tick runs every 60s, and per-minute git/gh
    // subprocess churn for a stamp the tick does not serve is load the
    // measurement never asked for.
    if matches!(mode, SweepMode::Full) {
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
            if let Err(err) = crate::roster_progress::refresh_row(
                &progress_path,
                &e.name,
                Path::new(&e.cwd),
                &now,
            ) {
                eprintln!(
                    "reconcile: roster-progress refresh failed for {}: {err}",
                    e.name
                );
            }
        }
    }

    // The outcome events stay on the full sweeps: the tick's product is the
    // fresh stamp, not an event, and a per-minute inconsistent/deferred/
    // done triple for the same rows is events.jsonl noise, not signal.
    if matches!(mode, SweepMode::Full) {
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
    }
    Ok(ReconcileSweepResult {
        registry,
        entries,
        outcome,
    })
}

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
    } = match run_reconcile_sweep(
        &ctx.home,
        &ctx.emitter,
        &|entry: &RegistryEntry| {
            match ctx.codex_threads.try_lock() {
                Ok(guard) => guard.contains_key(&entry.name),
                // An actor is mid insert/remove: hosted, so a race can never
                // settle a thread the map is about to name.
                Err(_) => true,
            }
        },
        SweepMode::Full,
    ) {
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
    let mut notify: Option<(String, String, bool)> = None;
    // Apply under the seq gate: a store-path report that landed on the row after
    // it became visible (but before this drain) set a >= seq; never regress it.
    let _ = state::update_registry(&ctx.home.registry_json(), |r| {
        if let Some((body, is_done)) = gate_inside_leg_onto_row(r, session_uuid, rep.clone()) {
            notify = Some((name.to_string(), body, is_done));
        }
    });
    if let Some((title, body, is_done)) = notify {
        let o = &ctx.opts;
        notify_badge(title, body, is_done, o.notify_on_blocked, o.notify_on_done);
    }
    let _ = ctx.emitter.emit(
        "inside_leg_buffer_flushed",
        &json!({"name": name, "session_id": session_uuid, "state": state_str, "seq": seq}),
    );
}

/// Which null-uuid row (if any) should adopt a full session uuid seen on an
/// inside-leg report.
enum UuidBackfill {
    None,
    One(usize),
    Ambiguous,
}

/// Find the `claude --bg` row awaiting its full session uuid. A bg spawn writes
/// the row with the 8-hex jobId in `short_id` (v9) but `claude_session_uuid:
/// null` -- the full uuid only arrives on the first inside-leg report, so until
/// it is backfilled `entry_holds_session` never matches and every report is
/// buffered-then-lost. Match a null-uuid claude row whose short-id is
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
    // Badge-transition notify intent: (title, body, is_done). Captured
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
        // to it. Ambiguous prefix -> no backfill (AC1-ERR).
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
            }
            // Any report is an observation; a matching one is the success case.
            entry.model_basis = Some("verified".to_string());
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
                let o = &ctx.opts;
                notify_badge(title, body, is_done, o.notify_on_blocked, o.notify_on_done);
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
    // a channel lookup over an unreadable registry reports the failed
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

/// The interval-gated maintenance sweeps (stale questions, park records),
/// split out for the file budget; each is stamp-gated and pause-aware.
pub(crate) mod sweeps;
pub(crate) use sweeps::{park_sweep, stale_sweep};
#[cfg(test)]
pub(crate) use sweeps::{parse_stale_sweep, PARK_SWEEP_INTERVAL_SECS, STALE_SWEEP_INTERVAL_SECS};

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
// Declared beside tests (not inside daemon_tests.rs): that aggregator is
// over the file budget and may only shrink.
#[cfg(test)]
#[path = "daemon/tests/pid_zombie_tests.rs"]
mod pid_zombie_tests;
