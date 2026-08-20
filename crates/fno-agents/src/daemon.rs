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
use crate::identity::canonical_handle;
use crate::paths::{self, AgentsHome};
use crate::protocol::{
    read_request, write_request, write_response, ErrorCode, Namespace, Request, Response,
};
use crate::state::{self, RegistryEntry};
use crate::AgentStatus;
use serde_json::{json, Map, Value};
use std::os::unix::fs::MetadataExt; // ino() for the bound-socket ownership check
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
    /// cwd the idle tick resolves `agents.dead_row_grace.<harness>` against
    /// (x-9de7 task 6). A `Duration` cannot be pre-resolved here the way
    /// `idle_exit` is: the grace is per-HARNESS, so the lookup happens once
    /// per row, at sweep time, not once at startup.
    pub dead_row_grace_cwd: PathBuf,
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
            dead_row_grace_cwd: PathBuf::from("."),
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
///
/// Since x-4c87 an unreadable registry is a startup failure, not an empty
/// roster: recovery ran on `unwrap_or_default()` reads, so a store the typed
/// reader could not decode made the daemon come up believing zero agents and
/// answer every later caller from that false zero.
pub fn recover(
    home: &AgentsHome,
    emitter: &EventEmitter,
) -> Result<RecoveryReport, state::StateError> {
    let mut report = RecoveryReport::default();
    let registry = load_registry_asserted(&home.registry_json())?;

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
        // Keyed on (short_id, pid), not short_id alone (x-9de7 task 1). Every
        // codex/gemini shellout row shares the same empty short_id, so a
        // short_id-only set condemns every row wearing that empty id the
        // moment ONE of them fails pid_is_ours -- including live pane-hosted
        // siblings that were never checked. pid is what pid_is_ours actually
        // verified, so it is what must gate the write.
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
    /// Rows removed on absolute age alone, with nothing to corroborate.
    ///
    /// Kept SEPARATE from `reaped` on purpose. A backstop folded into one total
    /// becomes the main path silently, and the corroboration gate it bypasses
    /// turns into decoration. Reported at every pass, including zero.
    pub reaped_backstop: Vec<String>,
    /// Live rows removed on a positive done reading (idle past grace +
    /// transcript tail classifies `done`). A THIRD count with its own meaning:
    /// a finished turn, not a death; the reap event carries the resumable
    /// handle for it. Separate from `reaped` for the same reason
    /// `reaped_backstop` is.
    pub reaped_dormant: Vec<String>,
    /// `(row id, reason)` for every reaped row whose harness-session cascade
    /// REFUSED or failed. Surfaced, never swallowed; the registry reap is not
    /// rolled back for any of them.
    pub cascade_refused: Vec<(String, String)>,
    /// Past-grace rows kept by the corroboration gate alone: no confirmed-dead
    /// pid, no positively-stale transcript, and a liveness surface still on
    /// record - short of the backstop horizon too (x-9de7 task 5). This is
    /// the "stuck and invisible" case `gc.rs`'s own comments warn about;
    /// before this field it had no report at all.
    pub kept_uncorroborated: Vec<String>,
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
    seen.into_iter().collect()
}

/// How long between worktree report sweeps. Long on purpose: this is the
/// backstop for what the merge-triggered ritual missed, not a control loop.
const WORKTREE_SWEEP_INTERVAL_SECS: u64 = 86_400;

/// One repo's worktree-sweep reading, parsed from the verb's `Summary:` line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeSweepReport {
    pub eligible: usize,
    pub kept: usize,
    pub dirty: usize,
}

/// Parse `fno worktree cleanup --merged`'s summary line.
///
/// Returns `None` rather than a zeroed report when the line is absent. A sweep
/// that could not read its own output must not report "0 eligible, 0 dirty",
/// which is indistinguishable from a clean machine: an absence has two
/// explanations and a count must only ever come from a real reading.
pub fn parse_worktree_sweep(stdout: &str) -> Option<WorktreeSweepReport> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("Summary:"))?;
    let num_before = |needle: &str| -> Option<usize> {
        let idx = line.find(needle)?;
        line[..idx].split_whitespace().last()?.parse().ok()
    };
    Some(WorktreeSweepReport {
        eligible: num_before(" would archive")?,
        kept: num_before(" kept (")?,
        dirty: num_before(" dirty")?,
    })
}

/// Report-only worktree sweep, one line per repo, on a 24h floor.
///
/// REPORTS, NEVER REMOVES, and that split is deliberate. A merged PR is external
/// proof the work landed; a timer tick proves nothing. Removal stays on the
/// merge-triggered path, gated by the existing `post_merge.self_reap`. There is
/// no second config knob, because two off-switches for one decision strand
/// whoever flips the wrong one.
///
/// `run` is injected so the policy is testable without shelling out.
pub fn worktree_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    now: i64,
    roots: &[String],
    run: &dyn Fn(&str) -> Option<String>,
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
        // Emit for EVERY repo, including the ones that read zero. A tick that
        // stays silent when it finds nothing cannot be told from a tick that
        // never ran, and this sweep exists precisely to surface what the
        // ritual missed.
        match run(root).as_deref().and_then(parse_worktree_sweep) {
            Some(r) => {
                let _ = emitter.emit(
                    "worktree_sweep",
                    &json!({
                        "repo": root,
                        "eligible": r.eligible,
                        "kept": r.kept,
                        "dirty": r.dirty,
                        "mode": "report-only",
                    }),
                );
                swept += 1;
            }
            None => {
                let _ = emitter.emit(
                    "worktree_sweep",
                    &json!({"repo": root, "mode": "report-only", "error": "unreadable-summary"}),
                );
            }
        }
    }
    let _ = std::fs::write(&stamp, now.to_string());
    swept
}

/// Has this worker's transcript been written recently enough to call it alive?
///
/// `Some(true)` touched within `window_secs`, `Some(false)` positively stale,
/// `None` no path recorded or the file cannot be stat'd.
///
/// This is the INDEPENDENT half of the reap decision. `status` and `exited_at`
/// are one signal wearing two hats: `gc_sweep` sets the stamp when a sweep first
/// fails to reach a worker, so they cannot corroborate each other. A claude bg
/// thread that finished a turn is idle and resumable, not gone, and a batched
/// stamp says nothing about which it is. Its transcript does.
///
/// `None` never grants permission. An unreadable transcript has two
/// explanations and only one of them is a dead worker.
fn transcript_fresh_probe(log_path: Option<&str>, now: i64, window_secs: i64) -> Option<bool> {
    let path = log_path.filter(|p| !p.is_empty())?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now.saturating_sub(secs) <= window_secs)
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
struct HarnessStoreIndex {
    /// Resolved store roots; `None` until the first lookup resolves them from
    /// `$HOME` (or forever, for an index built `with_roots` in tests).
    claude_root: Option<std::path::PathBuf>,
    codex_root: Option<std::path::PathBuf>,
    /// `(filename, path)` for every candidate file, or `None` until the first
    /// lookup walks the store. `Some(Err(()))` marks a walk that hit an
    /// unreadable directory: every later lookup answers None, fail closed.
    claude: Option<Result<Vec<(String, std::path::PathBuf)>, ()>>,
    codex: Option<Result<Vec<(String, std::path::PathBuf)>, ()>>,
    claude_agents: Option<crate::claude_roster::ClaudeAgentsSnapshot>,
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
                _ => Some(home.join(".codex").join("sessions")),
            }
        })
    }

    /// Every transcript candidate this row's harness store holds for its
    /// session id. Empty vector = the session is GONE from its own store.
    fn matches(&mut self, e: &state::RegistryEntry) -> Option<Vec<std::path::PathBuf>> {
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
                    _ => name.starts_with("rollout-") && name.contains(sid),
                })
                .map(|(_, p)| p.clone())
                .collect(),
        )
    }

    fn claude_agents(&mut self) -> &crate::claude_roster::ClaudeAgentsSnapshot {
        self.claude_agents
            .get_or_insert_with(crate::claude_roster::read_all_agents)
    }
}

/// Bounded walk collecting `(filename, path)` for every regular file under
/// `dir` (claude is two levels, codex four; depth 5 covers both). `Err` on any
/// unreadable directory: an unreadable store answers nothing, fail closed.
fn index_tree(
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

/// The most recently written of the store's matches, for the freshness probe.
/// Newest, not first: a session can leave stubs in other project dirs, and a
/// stub whose creation post-dates the real transcript's last turn must not
/// read as the fresher one is stale (a misread there only KEEPS a row, the
/// fail-safe direction).
fn newest_by_mtime(paths: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    paths
        .iter()
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
        .cloned()
}

/// Truth probes spent on live-idle rows per sweep. Bounded so a registry full
/// of idle live rows cannot turn one sweep into an unbounded subprocess farm;
/// the remainder is candidates on the next tick.
const DORMANT_PROBE_CAP: usize = 8;
/// Harness-session cascades per sweep, same rationale as DORMANT_PROBE_CAP.
const CASCADE_CAP: usize = 10;
/// Wall-clock bound for one harness removal subprocess. A hung removal must
/// never wedge the sweep (the operator measured a 300s+ hang on a stuck row;
/// the cascade cannot inherit it).
const CASCADE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long since this row last showed activity, in seconds. `None` when no
/// activity signal can be read at all (no `last_message_at`, no stat-able
/// transcript): idleness cannot be PROVEN then, and only a positive idle
/// reading opens the dormant gate.
fn row_idle_secs(
    e: &state::RegistryEntry,
    now: i64,
    transcript: Option<&std::path::Path>,
) -> Option<i64> {
    let msg = e
        .last_message_at
        .as_deref()
        .and_then(state::rfc3339_like_to_secs)
        .map(|s| s as i64);
    let transcript = transcript.and_then(|p| {
        std::fs::metadata(p)
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    });
    let latest = [msg, transcript].into_iter().flatten().max()?;
    Some(now.saturating_sub(latest))
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

    fn failure(&self, row_id: &str) -> Option<(String, String)> {
        match self {
            Self::Failed(reason) => Some((row_id.to_string(), reason.clone())),
            Self::Unverified(reason) => Some((
                row_id.to_string(),
                format!("harness teardown unverified: {reason}"),
            )),
            _ => None,
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
        _ => CascadeOutcome::NotApplicable,
    }
}

fn cascade_harness_session_with(
    index: &mut HarnessStoreIndex,
    e: &state::RegistryEntry,
) -> Option<(String, String)> {
    let row_id = claude_row_id(e).unwrap_or_else(|| e.name.clone());
    let snapshot = if e.harness_name() == "claude" {
        Some(index.claude_agents().clone())
    } else {
        None
    };
    cascade_harness_session_result_with(
        e,
        snapshot.as_ref(),
        &crate::claude_roster::read_all_agents,
        &run_claude_rm,
    )
    .failure(&row_id)
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
fn is_linked_worktree(cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    std::path::Path::new(cwd).join(".git").is_file()
}

/// Can this worktree-owning row's `cwd` be removed without destroying work?
/// `Some(true)` yes, `Some(false)` no, `None` the probe could not determine it
/// -> the caller fails closed and keeps the row.
///
/// Routes through `fno worktree reapable`, the same answer the `--merged` sweep
/// and `archive-worktree.sh` use, so three call sites cannot drift apart (an
/// equivalence test pins that they agree). The old rule here was "is
/// `git status --porcelain` empty", which blocked on a tracked file merely
/// MISSING from disk - content HEAD still holds, so removal loses nothing.
///
/// Permission needs BOTH a clean exit and the literal `reapable=yes` marker. A
/// stale `fno` predating the verb exits non-zero with no receipt, which is
/// indistinguishable from any other non-answer, so every unknown degrades to
/// `None` and the row is kept. That is exactly the prior behaviour.
fn worktree_clean_probe(cwd: &str) -> Option<bool> {
    let out = std::process::Command::new("fno")
        .current_dir(cwd)
        .args(["worktree", "reapable", cwd])
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

/// Wall-clock epoch seconds, for GC grace math. Degrades to 0 (a pre-1970 clock
/// makes every stamped row look in-grace -> nothing reaped, the safe direction).
fn now_epoch_secs() -> i64 {
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
fn dispatch_node_id(name: &str) -> Option<String> {
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
enum DispatchTermination {
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

fn dispatch_termination(
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
        crate::loop_runtime::ProjectJournalPath(
            crate::paths::worktree_repo_root(Path::new(&entry.cwd))
                .join(".fno")
                .join("events.jsonl"),
        ),
        crate::loop_runtime::GlobalJournalPath(global_events_path(home)),
    );
    match journal.find_termination_strict(&session_id) {
        Ok(Some(_)) => DispatchTermination::Found(session_id),
        Ok(None) => DispatchTermination::Absent(Some(session_id)),
        Err(err) => DispatchTermination::Unknown(err.to_string()),
    }
}

fn record_dead_dispatch(
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

fn restore_unaccounted_row(home: &AgentsHome, entry: &RegistryEntry) -> Result<(), String> {
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
/// `grace_for_harness` resolves `agents.dead_row_grace` PER ROW, keyed on
/// `e.harness_name()` (x-9de7 task 6) -- defence in depth, not the fix: it
/// only sets the blast radius of a false `exited` write, since a live row is
/// re-checked and never touched regardless of grace. Injected so this stays
/// testable without shelling config reads; production passes
/// `agents_config::dead_row_grace_secs`.
pub fn gc_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_for_harness: &dyn Fn(&str) -> Duration,
) -> GcSummary {
    let store = std::cell::RefCell::new(HarnessStoreIndex::default());
    let cascade_store = std::cell::RefCell::new(HarnessStoreIndex::default());
    gc_sweep_impl(
        home,
        emitter,
        grace_for_harness,
        false,
        &live_truth_tail_state,
        &|e| store.borrow_mut().matches(e),
        &|e| cascade_harness_session_with(&mut cascade_store.borrow_mut(), e),
    )
}

/// `fno agents reap --dry-run` (x-9de7 task 5): classify the registry exactly
/// as [`gc_sweep`] does, including every `kept_dirty` / `kept_uncorroborated`
/// diagnostic, but never write the registry, never emit a `daemon_recovery_error`
/// or `agent_row_reaped` event, and never touch dispatch termination. "Would
/// reap" and "would keep, and why" are read straight off the same
/// classification the real sweep uses - a reaper an operator cannot rehearse
/// is one they will not run.
pub fn gc_sweep_dry_run(
    home: &AgentsHome,
    grace_for_harness: &dyn Fn(&str) -> Duration,
) -> GcSummary {
    // Never emitted to in dry-run mode (the whole write+emit tail is skipped
    // below), so an unused placeholder path satisfies the shared signature.
    let emitter = EventEmitter::new(std::path::PathBuf::new(), "daemon");
    let store = std::cell::RefCell::new(HarnessStoreIndex::default());
    let cascade_store = std::cell::RefCell::new(HarnessStoreIndex::default());
    gc_sweep_impl(
        home,
        &emitter,
        grace_for_harness,
        true,
        &live_truth_tail_state,
        &|e| store.borrow_mut().matches(e),
        &|e| cascade_harness_session_with(&mut cascade_store.borrow_mut(), e),
    )
}

/// The dormant gate's transcript-tail read, as production runs it: the shared
/// truth probe (`fno agents truth <handle> --json`, bounded at 5s), lowered to
/// the state string alone. Injected into [`gc_sweep_impl`] so the decision
/// path is unit-testable without shelling out to a real `fno`.
fn live_truth_tail_state(handle: &str) -> Option<String> {
    crate::claude_ask::family1_truth_probe(handle).map(|p| p.state)
}

fn gc_sweep_impl(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_for_harness: &dyn Fn(&str) -> Duration,
    dry_run: bool,
    truth_tail_state: &dyn Fn(&str) -> Option<String>,
    // The harness-store lookup (every transcript candidate this row's own
    // store holds for its session id), injected so a sweep-level test never
    // depends on what lives in the developer's real ~/.claude / ~/.codex.
    store_matches: &dyn Fn(&state::RegistryEntry) -> Option<Vec<std::path::PathBuf>>,
    // The post-reap harness-store cascade, injected for the same reason: a
    // test must be able to stage a refusal without mutating PATH/HOME.
    cascade: &dyn Fn(&state::RegistryEntry) -> Option<(String, String)>,
) -> GcSummary {
    let mut summary = GcSummary::default();
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();
    if registry.entries.is_empty() {
        return summary; // empty registry -> nothing to sweep (Boundary)
    }
    let live_workers = home.scan_worker_sockets();
    let now = now_epoch_secs();

    // Keyed by row name -> the `created_at` we evaluated. Applied under the lock
    // ONLY when the row's current `created_at` still matches, so a same-name
    // session reaped-and-recreated (or resurrected) between this unlocked snapshot
    // + the slow git probes and the exclusive write is never clobbered by a
    // stale name-only decision (TOCTOU; gemini HIGH / codex P2 on PR #126).
    // `created_at` is the spawn-stamped identity discriminant: a replacement
    // session carries a fresh one.
    let mut to_reap: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    // Rows in `to_reap` that got there on age alone, keyed by registry name.
    let mut backstop_ids: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    // Rows in `to_reap` that got there on a live-but-done reading, keyed by
    // registry name. Reported separately (a death and a finished turn must stay
    // distinguishable) and carrying a resumable handle in the reap event.
    let mut dormant_ids: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    // Truth probes spent on live-idle rows this sweep (see DORMANT_PROBE_CAP).
    let mut dormant_probes: usize = 0;
    let mut to_stamp: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut to_clear: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for e in &registry.entries {
        let grace_secs = grace_for_harness(e.harness_name()).as_secs() as i64;
        let is_live = live_workers.contains(&e.short_id)
            || e.pid
                .map(|p| pid_is_ours(p, e.pid_start_time))
                .unwrap_or(false);
        let pid_confirmed_dead = e
            .pid
            .map(|p| !pid_is_ours(p, e.pid_start_time))
            .unwrap_or(false);
        // A one-shot ask owns nothing; neither does a row sitting in the
        // canonical checkout or in a plain directory. Only a LINKED worktree is
        // removable, and only there does cleanliness decide anything.
        let owns_worktree = !e.is_one_shot_ask() && is_linked_worktree(&e.cwd);
        let exited_at = e
            .exited_at
            .as_deref()
            .and_then(state::rfc3339_like_to_secs)
            .map(|s| s as i64);

        // Probe the worktree only for a row that could actually be reaped this
        // pass (dead + terminal + past grace + owns a worktree). Keeps git off the
        // hot path: steady state has no such rows, so no subprocess runs.
        // The terminal set comes from the POLICY's one spelling: gating probes
        // on a narrower local copy strands exactly the rows the policy would
        // remove (the Orphaned drift).
        let terminal_or_dead = crate::gc::status_is_terminal(e.status) || pid_confirmed_dead;
        let past_grace = matches!(exited_at, Some(t) if now.saturating_sub(t) > grace_secs);
        // The second signal, read whenever a reap is otherwise on the table.
        // Cheap (one stat), and it is the discrimination `status` cannot make:
        // an idle-but-resumable bg thread keeps touching its transcript.
        // Repointed at the harness's own transcript: fno's log copies are
        // routinely absent (83 of 88 rows on the machine this was measured
        // on), which used to kill the freshness signal outright - it read
        // `None` (unknown) and fail-closed to Keep for want of a file nobody
        // writes anymore. `log_path` remains the fallback for a row with no
        // session id.
        // ONE store lookup answers both the existence question (gone?) and
        // the freshness question (newest match's mtime) for this row.
        let store_hits = if !is_live && terminal_or_dead && past_grace {
            store_matches(e)
        } else {
            None
        };
        let harness_session_gone = store_hits.as_ref().map(|m| m.is_empty());
        let transcript_fresh = if !is_live && terminal_or_dead && past_grace {
            let harness_path = store_hits.as_deref().and_then(newest_by_mtime);
            match harness_path
                .as_deref()
                .and_then(|p| p.to_str())
                .or_else(|| e.log_path.as_deref())
            {
                Some(path) => transcript_fresh_probe(Some(path), now, grace_secs),
                None => None,
            }
        } else {
            None
        };
        // Live-but-done (the third eviction route): a live row idle past the
        // grace window whose transcript tail POSITIVELY classifies done
        // (promise emitted). A credential-dead worker reads live too, and
        // neither alive nor dead; only the positive done reading evicts.
        // Bounded: only idle rows are probed, and at most DORMANT_PROBE_CAP
        // truth probes run per sweep, so a large registry cannot turn the
        // sweep into a subprocess farm.
        let mut dormant_done = false;
        if is_live && dormant_probes < DORMANT_PROBE_CAP {
            // The idle gate's transcript read comes from the same store index
            // (in memory after the first build), never a fresh walk.
            let transcript = store_matches(e)
                .and_then(|m| newest_by_mtime(&m))
                .or_else(|| e.log_path.as_deref().map(std::path::PathBuf::from));
            if let Some(idle) = row_idle_secs(e, now, transcript.as_deref()) {
                if idle > grace_secs {
                    let handle = if e.short_id.is_empty() {
                        e.name.as_str()
                    } else {
                        e.short_id.as_str()
                    };
                    // Count the attempt, not just a successful answer: a timed-out
                    // or unreadable probe still costs the sweep its bounded
                    // subprocess budget, and a registry full of unresponsive
                    // sessions would otherwise pay that cost on every idle row
                    // with the cap never engaging (codex review, PR #889).
                    dormant_probes += 1;
                    if let Some(state) = truth_tail_state(handle) {
                        dormant_done = state == "done";
                    }
                }
            }
        }
        // Built with `worktree_clean` unset so the probe decision can ask the
        // policy itself. Filled in below, before any verdict is read from it.
        let mut row = crate::gc::GcRow {
            status: e.status,
            is_live,
            pid_confirmed_dead,
            owns_worktree,
            exited_at,
            // A one-shot ask carries neither pid nor short_id: no worker can be
            // hiding behind an identity that was never recorded.
            liveness_surface: e.pid.is_some() || !e.short_id.is_empty(),
            transcript_fresh,
            harness_session_gone,
            dormant_done,
            worktree_clean: None,
        };
        // The probe condition MIRRORS the removal condition, asked through the
        // one function that defines it. A narrower test here strands any row the
        // policy would remove: `worktree_clean` stays `None`, the fail-closed arm
        // keeps it, and the `kept_dirty` line below is gated on this same flag,
        // so the operator is told nothing either. A backstop row is the other
        // half - it has no corroboration by definition, and without it the valve
        // never opens for the worktree-owning rows it was ordered for.
        let past_backstop = matches!(exited_at,
            Some(t) if now.saturating_sub(t) > crate::gc::backstop_horizon_secs(grace_secs));
        let needs_probe = !is_live
            && terminal_or_dead
            && past_grace
            && owns_worktree
            && (crate::gc::removal_is_corroborated(&row) || past_backstop);
        if needs_probe {
            row.worktree_clean = worktree_clean_probe(&e.cwd);
        }
        let id = if e.short_id.is_empty() {
            e.name.clone()
        } else {
            e.short_id.clone()
        };
        match crate::gc::gc_action(&row, now, grace_secs) {
            crate::gc::GcAction::Reap => {
                to_reap.insert(e.name.clone(), e.created_at.clone());
            }
            crate::gc::GcAction::ReapBackstop => {
                to_reap.insert(e.name.clone(), e.created_at.clone());
                backstop_ids.insert(e.name.clone(), id.clone());
            }
            crate::gc::GcAction::ReapDormant => {
                to_reap.insert(e.name.clone(), e.created_at.clone());
                dormant_ids.insert(e.name.clone(), id.clone());
            }
            crate::gc::GcAction::StampExit => {
                to_stamp.insert(e.name.clone(), e.created_at.clone());
            }
            crate::gc::GcAction::Keep => {
                if is_live && e.exited_at.is_some() {
                    // Resurrected: drop the stale exit stamp so a later death
                    // starts a fresh grace clock.
                    to_clear.insert(e.name.clone(), e.created_at.clone());
                } else {
                    // The SAME decision `gc_action` above just made, read a
                    // second time for its reason (x-9de7 task 5): a row that
                    // is stuck and invisible is the failure mode `gc.rs`'s own
                    // comments warn about, so a past-grace Keep always gets a
                    // named gate instead of a silent, unexplained keep.
                    match crate::gc::keep_reason(&row, now, grace_secs) {
                        Some(
                            crate::gc::KeepReason::WorktreeDirty
                            | crate::gc::KeepReason::WorktreeUnprobed,
                        ) => {
                            summary.kept_dirty.push((id, e.cwd.clone()));
                        }
                        Some(crate::gc::KeepReason::Uncorroborated) => {
                            summary.kept_uncorroborated.push(id);
                        }
                        // Live / NotTerminal / WithinGrace: the ordinary,
                        // expected keep - not what task 5 exists to surface.
                        _ => {}
                    }
                }
            }
        }
    }

    // Cap the whole reap batch at CASCADE_CAP, not just the cascade calls
    // within it: every row removed from the registry below MUST get its
    // harness-store cascade attempted THIS sweep, because a row past that
    // point cannot become a cascade candidate again (its registry record,
    // the only handle the next sweep would find it by, is gone). A row
    // beyond the cap simply stays in the registry and is re-evaluated next
    // tick (codex review, PR #889). Truncated deterministically by name so
    // dry-run and the real write agree on exactly which rows this covers.
    if to_reap.len() > CASCADE_CAP {
        let keep: std::collections::BTreeMap<String, String> = to_reap
            .iter()
            .take(CASCADE_CAP)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        to_reap = keep;
    }

    if to_reap.is_empty() && to_stamp.is_empty() && to_clear.is_empty() {
        return summary;
    }
    if dry_run {
        // "Would reap": same `to_reap`/`backstop_ids` membership the real
        // write below applies, read straight off the classification pass
        // with no lock taken and no disk touched - `--dry-run`'s whole
        // contract. Stamp/clear candidates need no report: they never remove
        // a row, so a rehearsal has nothing to say about them.
        for e in &registry.entries {
            if to_reap.get(&e.name) != Some(&e.created_at) {
                continue;
            }
            let id = if e.short_id.is_empty() {
                e.name.clone()
            } else {
                e.short_id.clone()
            };
            if backstop_ids.contains_key(&e.name) {
                summary.reaped_backstop.push(id);
            } else if dormant_ids.contains_key(&e.name) {
                summary.reaped_dormant.push(id);
            } else {
                summary.reaped.push(id);
            }
        }
        return summary;
    }

    let now_stamp = now_rfc3339_like();
    // Names actually removed under the lock (identity still matched), so the emit
    // + summary report only what really happened (AC1-ERR / no phantom reaps).
    let mut reaped_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Harness-store cascades performed this sweep (see CASCADE_CAP).
    let mut cascaded: usize = 0;
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
                    let node_id = dispatch_node_id(&e.name);
                    let mut target_session_id = None;
                    let mut termination_event = false;
                    let mut accounted = true;
                    if let Some(node_id) = node_id.as_deref() {
                        match dispatch_termination(home, e, node_id) {
                            DispatchTermination::Found(session_id) => {
                                target_session_id = Some(session_id);
                                termination_event = true;
                            }
                            DispatchTermination::Absent(session_id) => {
                                target_session_id = session_id;
                                if let Err(err) = record_dead_dispatch(
                                    home,
                                    e,
                                    node_id,
                                    target_session_id.as_deref(),
                                ) {
                                    accounted = false;
                                    let restore = restore_unaccounted_row(home, e);
                                    let _ = emitter.emit(
                                        "daemon_recovery_error",
                                        &json!({
                                            "op": "record_dead_dispatch",
                                            "short_id": e.short_id,
                                            "error": err,
                                            "restore_error": restore.err(),
                                        }),
                                    );
                                }
                            }
                            DispatchTermination::Unknown(err) => {
                                accounted = false;
                                let restore = restore_unaccounted_row(home, e);
                                let _ = emitter.emit(
                                    "daemon_recovery_error",
                                    &json!({
                                        "op": "observe_dead_dispatch_termination",
                                        "short_id": e.short_id,
                                        "error": err,
                                        "restore_error": restore.err(),
                                    }),
                                );
                            }
                        }
                    }
                    if !accounted {
                        continue;
                    }
                    // CASCADE (AC6): two stores, act on both, report both.
                    // Deferred until AFTER dispatch accounting confirms this
                    // row - cascading first and failing accounting second
                    // would restore the registry row while its harness
                    // session (or codex index entry) is already gone,
                    // leaving a restored-but-unresumable row (codex review,
                    // PR #889). If the harness's own store still holds the
                    // session, remove it via the harness's own removal
                    // surface. A refusal or failure is SURFACED in the reap
                    // report and event (`cascade_refused`), never swallowed,
                    // and never rolls back the registry reap - but an
                    // unknown harness store (probe None) is a skip, not a
                    // refusal: registry-only rows are the opencode/gemini
                    // contract, not a failure. Bounded per sweep (the whole
                    // reap batch is already capped at CASCADE_CAP above, so
                    // this bound never actually engages - kept as a
                    // belt-and-suspenders invariant, not the enforcement
                    // point).
                    let mut cascade_refused: Option<(String, String)> = None;
                    if cascaded < CASCADE_CAP {
                        cascaded += 1;
                        cascade_refused = cascade(e);
                    }
                    if let Some((id, reason)) = &cascade_refused {
                        summary.cascade_refused.push((id.clone(), reason.clone()));
                    }
                    let _ = emitter.emit_fields(
                        "agent_row_reaped",
                        json_obj(&[
                            ("short_id", Value::String(e.short_id.clone())),
                            ("name", Value::String(e.name.clone())),
                            (
                                "node_id",
                                node_id.clone().map_or(Value::Null, Value::String),
                            ),
                            (
                                "session_id",
                                target_session_id.map_or(Value::Null, Value::String),
                            ),
                            ("termination_event", Value::Bool(termination_event)),
                            ("harness", Value::String(e.harness_name().to_string())),
                            (
                                "harness_session_id",
                                e.harness_session_id
                                    .clone()
                                    .map_or(Value::Null, Value::String),
                            ),
                            // A dormant reap is a finished turn, not a death:
                            // the resumable handle (harness + session id above)
                            // is the whole point of recording it.
                            ("resumable", Value::Bool(dormant_ids.contains_key(&e.name))),
                            (
                                "cascade_refused",
                                match &cascade_refused {
                                    Some((id, reason)) => json!({"id": id, "reason": reason}),
                                    None => Value::Null,
                                },
                            ),
                        ]),
                    );
                    let reaped_id = if e.short_id.is_empty() {
                        e.name.clone()
                    } else {
                        e.short_id.clone()
                    };
                    // A backstop removal is counted ONLY in its own list, never
                    // in both. Two totals that overlap cannot be compared, and
                    // comparing them is the point. A dormant reap likewise: it
                    // is a third count with its own meaning.
                    if backstop_ids.contains_key(&e.name) {
                        summary.reaped_backstop.push(reaped_id);
                    } else if dormant_ids.contains_key(&e.name) {
                        summary.reaped_dormant.push(reaped_id);
                    } else {
                        summary.reaped.push(reaped_id);
                    }
                }
            }
        }
        Err(err) => {
            let _ = emitter.emit(
                "daemon_recovery_error",
                &json!({"op": "gc_sweep", "error": err.to_string()}),
            );
            // Nothing was removed; report no reaps (no event/disk divergence).
            // ALL THREE lists, or the next writer that populates them earlier
            // leaves this path claiming zero ordinary reaps beside phantom
            // backstop or dormant ones - counts that disagree about the same
            // failed sweep.
            summary.reaped.clear();
            summary.reaped_backstop.clear();
            summary.reaped_dormant.clear();
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
    // exact defect this guard removes, once at every `fno update`.
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
    let report = recover(&home, &emitter)?;

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
                        run_reconcile_sweep(&home_sweep, &emitter_sweep)
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
    // Dead-row GC gate (x-ef7f): its dormant check shells one truth probe per
    // registry row, so it gets the same one-in-flight discipline as the sweeps
    // beside it rather than running inline in the select arm.
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
                        let _gate = SweepGate(flag);
                        crate::scrape::scrape_sweep(&home, &emitter, notify_on_blocked);
                    });
                }
                // Dead-row GC (x-b1aa): remove terminal, past-grace, clean
                // agent-view rows so finished rows self-clean without the merge
                // ritual. Cheap in steady state (no candidates -> no git, no
                // registry write); the grace window makes exact cadence
                // non-critical, so running it on the idle tick is fine.
                // Runs off-loop under spawn_blocking behind a one-in-flight
                // gate, like the scrape and worktree sweeps above (x-ef7f). Its
                // dormant gate shells `fno agents truth` ONCE PER REGISTRY ROW,
                // each child bounded at 5s and retried once on a crash, so a
                // 28-row roster could hold this select arm for minutes at a
                // time. Inline, that starved accept() -- clients' connects timed
                // out and lazy-started competing daemons, each adding rows and
                // lengthening the next sweep -- and it starved the SIGTERM arm
                // beside it, which is why a wedged daemon could only be
                // SIGKILLed.
                if !gc_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&gc_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    let grace_cwd = ctx.opts.dead_row_grace_cwd.clone();
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        let grace_for_harness = |harness: &str| {
                            Duration::from_secs(crate::agents_config::dead_row_grace_secs(
                                &grace_cwd, harness,
                            ))
                        };
                        let _ = gc_sweep(&home, &emitter, &grace_for_harness);
                    });
                }
                // Worktree report sweep: the backstop for what the merge ritual
                // missed. Its own 24h stamp makes it a near-no-op on this tick,
                // but the verb shells git across every worktree when it does
                // fire, so it runs off-loop behind a one-in-flight gate like the
                // scrape sweep. Report-only by construction.
                if !worktree_sweep_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&worktree_sweep_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    tokio::task::spawn_blocking(move || {
                        let _gate = SweepGate(flag);
                        let roots = registry_repo_roots(&home);
                        let now = now_epoch_secs();
                        worktree_sweep(&home, &emitter, now, &roots, &|root| {
                            std::process::Command::new("fno")
                                .current_dir(root)
                                .args(["worktree", "cleanup", "--merged"])
                                .output()
                                .ok()
                                .filter(|o| o.status.success())
                                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        });
                    });
                }
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
}

/// Cap on the early-push buffer (E3.3). A report for a NEW session is dropped
/// (logged `buffer_full`) once the buffer is at cap; an already-buffered
/// session's seq still advances (no new key). 64 covers any realistic burst of
/// panes registering at once while staying a hard ceiling.
const PENDING_INSIDE_LEG_CAP: usize = 64;

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
struct SweepGate(Arc<std::sync::atomic::AtomicBool>);

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
        Some("switchboard") | Some("switchboard_v2") => handle_switchboard(ctx, req).await,
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
        legacy_provider: String::new(),
        provider: None,
        model: None,
        effort: None,
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
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
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

    let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
        Ok(r) => r,
        Err(e) => return registry_read_failed(req.id, e),
    };
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
                        // content scan prohibits (check-axis-vocabulary).
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
/// `pid_confirmed_live` is a positive measurement in its OWN right (x-9de7
/// task 3): a row whose `short_id` and `harness_session_id` are both empty
/// resolves to its bare `name` in `registry_truth_handle`, which the truth
/// probe can never find, so it answers `unknown` -- unrelated to whether the
/// worker is actually running. Only the two shapes that mean "the probe could
/// not measure" (`Some("unknown")`, and the state fallthrough) accept the
/// override; `reachable`/`unreachable` and `working`/`done`/`stalled` stay
/// probe-authoritative and unchanged, matching the monotone-lowering rule
/// (never let a weaker signal raise a row the probe positively lowered).
fn rendered_status_from_truth(
    probe: Option<&crate::claude_ask::TruthProbe>,
    pid_confirmed_live: bool,
) -> &'static str {
    match probe.and_then(|p| p.reachability.as_deref()) {
        Some("reachable") => return "live",
        Some("unreachable") => return "orphaned",
        Some("unknown") => {
            return if pid_confirmed_live {
                "live"
            } else {
                "unknown"
            }
        }
        _ => {}
    }
    match probe.map(|p| p.state.as_str()) {
        Some("working" | "watching" | "your-move") => "live",
        Some("done" | "stalled") => "orphaned",
        _ if pid_confirmed_live => "live",
        _ => "unknown",
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
    probe: Option<&crate::claude_ask::TruthProbe>,
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

fn handle_list(ctx: &Ctx, req: &Request) -> Response {
    handle_list_with_truth(ctx, req, crate::claude_ask::family1_truth_probe)
}

/// The attention window this surface orders by. Session-truth's stall window
/// is 7200s and correct FOR REAPING; for display it is exactly the gap a
/// dead-under-two-hours worker hides in, so the ordering window is ten
/// minutes. Mirrors `session_truth.STALE_ATTENTION_S` and the mux client's
/// constant - the three cannot share code (the crates do not link), so the
/// shared fixture in schemas/ is what pins them together.
const STALE_ATTENTION_S: f64 = 600.0;

/// One row's list-lane attention key: evidence tier, then longest-silent
/// first, then name so consecutive lists never shuffle equal rows. Only
/// fields that carry their evidence with them (`basis`,
/// `last_activity_age_s`) - never `status`, never a bare verdict. A row with
/// no probe answer (all three null) lands in the neutral tier with age 0:
/// absence of a reading is not urgency.
/// `to_bits` is order-preserving for non-negative f64 (and an age is a
/// duration, always non-negative), which is what lets a float age ride an
/// `Ord` tuple key.
fn attention_sort_key(row: &Value) -> (u8, std::cmp::Reverse<u64>, String) {
    let basis = row.get("basis").and_then(|v| v.as_str());
    let age = row
        .get("last_activity_age_s")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let tier = if matches!(basis, Some("process-gone") | Some("pane-gone"))
        || row.get("reachability").and_then(|v| v.as_str()) == Some("unreachable")
    {
        5
    } else if basis == Some("transcript") && age >= STALE_ATTENTION_S {
        0
    } else if basis == Some("silent") {
        1
    } else if basis == Some("no-evidence") {
        2
    } else {
        4
    };
    (
        tier,
        std::cmp::Reverse(age.to_bits()),
        row.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

fn handle_list_with_truth<F>(ctx: &Ctx, req: &Request, truth_fn: F) -> Response
where
    F: Fn(&str) -> Option<crate::claude_ask::TruthProbe>,
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
        if st != "live" && st != "orphaned" && st != "unknown" {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!("invalid --status '{st}' (expected: live | orphaned | unknown)"),
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
    let classified: Vec<_> = registry
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
                if e.harness_name() != prov.as_str() {
                    return false;
                }
            }
            true
        })
        .map(|e| {
            let truth_handle = registry_truth_handle(e);
            let truth = truth_fn(&truth_handle);
            let pid_confirmed_live = e
                .pid
                .map(|p| pid_is_ours(p, e.pid_start_time))
                .unwrap_or(false);
            let rendered_status = rendered_status_from_truth(truth.as_ref(), pid_confirmed_live);
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
                json!({
                    "name": e.name,
                    // `harness` is the sole identity axis, and it names the CLI,
                    // never the model vendor. The `provider` alias that sat beside
                    // it carried this same harness value, so a worker routed to
                    // another vendor still listed `provider: claude` and read as
                    // proof the route had fallen back. `observed_model`
                    // below is the honest answer to that question.
                    "harness": e.harness_name(),
                    "harness_session_id": e.harness_session_id,
                    "short_id": short_id,
                    "session_id": session_id,
                    "address": address,
                    "cwd": e.cwd,
                    "created_at": e.created_at,
                    "last_message_at": e.last_message_at,
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
                    // The mux hosting ref ({session, pane_id}) for a pane-hosted row,
                    // else null. A pane row's short_id is empty, so this is the only
                    // key that says where such a worker actually lives; without it a
                    // caller reads a bound pane worker as unhosted.
                    "mux": e.mux,
                    // Crown (US9): the compact descriptor plus the raw fields, so a
                    // minion can resolve who to escalate to.
                    "crown": crown,
                    "crown_level": e.crown_level,
                    "crown_scope": e.crown_scope,
                    "crown_grantor": e.crown_grantor,
                    // Superset of Python's serialize_entry: project_root is retained
                    // as the daemon's native grouping key (existing daemon_e2e
                    // contract) alongside the shared parity fields. Python list
                    // has no project_root; the extra key is a harmless superset.
                    "project_root": e.project_root,
                })
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
async fn stop_worker_confirmed(ctx: &Ctx, entry: &RegistryEntry) -> bool {
    let sock = ctx.home.worker_sock(&entry.short_id);
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
                     to stop; note that `rm` clears the row but does not stop a session"
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

async fn handle_rm(ctx: &Ctx, req: &Request) -> Response {
    handle_rm_with(
        ctx,
        req,
        &crate::claude_roster::read_all_agents,
        &run_claude_rm,
        &run_mux_pane_kill,
    )
    .await
}

async fn handle_rm_with(
    ctx: &Ctx,
    req: &Request,
    read_claude_agents: &(dyn Fn() -> crate::claude_roster::ClaudeAgentsSnapshot + Sync),
    claude_rm: &(dyn Fn(&str) -> Result<(), String> + Sync),
    mux_pane_kill: &(dyn Fn(&str, u64) -> Result<bool, String> + Sync),
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
    // The stored enum is what fno last WROTE, not what is true: a claude
    // session torn down by hand with `claude stop`/`claude rm` never
    // updates it. A claude row absent from the `claude agents --json --all`
    // roster is provably gone, whoever removed it. Anything less than proof
    // keeps refusing. This reconciliation is claude-only (codex/opencode
    // support is unimplemented, self-review finding): `provably_gone`
    // is unconditionally `false` for those harnesses, so they still hit the
    // pre-fix stale-status refusal below with `--force` as the only escape.
    let provably_gone =
        claude_row_provably_absent(claude_agents.as_ref(), harness_row_id.as_deref());
    if entry.status == AgentStatus::Live && !force && !provably_gone {
        let row = harness_row_id
            .clone()
            .unwrap_or_else(|| "(no harness row id)".into());
        let roster_known = claude_agents.as_ref().is_some_and(|snap| snap.is_known());
        let detail = if entry.harness_name() != "claude" {
            format!(
                "agent {name} is still live. Stop it with `fno agents stop {name}`, or pass \
                 --force."
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
                 with `fno agents stop {name}`, or pass --force."
            )
        } else if roster_known {
            format!(
                "agent {name} is still live. Its harness row {row} is present in \
                 `claude agents --json --all`. Stop it with `fno agents stop {name}`, or \
                 by hand with `claude stop {row}` then `claude rm {row}` (claude takes the \
                 SHORT ID {row}, never the agent name {name}). rm proceeds on its own \
                 once that row is gone."
            )
        } else {
            format!(
                "agent {name} is still live, and its harness row {row}'s presence in \
                 `claude agents --json --all` could not be confirmed (the roster read \
                 failed). Retry once the roster is readable, or pass --force."
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
    let pane_session = entry.mux.as_ref().map(|mux| mux.session.clone());
    let pane_id = entry.mux.as_ref().map(|mux| mux.pane_id);
    let event = json!({
        "name": name,
        "registry_removed": true,
        "harness": entry.harness_name(),
        "harness_row_id": harness_row_id,
        "harness_removed": harness_outcome.removed_json(),
        "harness_reason": harness_outcome.reason(),
        "pane_session": pane_session,
        "pane_id": pane_id,
        "pane_removed": pane_outcome.removed_json(),
        "pane_reason": pane_outcome.reason(),
        "was_orphaned": was_orphaned,
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
        "harness": entry.harness_name(),
        "harness_row_id": harness_row_id,
        "harness_removed": harness_outcome.removed_json(),
        "harness_reason": harness_outcome.reason(),
        "pane_session": pane_session,
        "pane_id": pane_id,
        "pane_removed": pane_outcome.removed_json(),
        "pane_reason": pane_outcome.reason(),
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
fn plan_reconcile<P, D, L, B>(
    entries: &[RegistryEntry],
    mut probe: P,
    mut budget_exhausted: D,
    mut pid_live: L,
    mut bg_live: B,
) -> (Vec<ReconcileChange>, ReconcileOutcome)
where
    P: FnMut(&RegistryEntry) -> Result<bool, crate::provider::ReachabilityProbeError>,
    D: FnMut() -> bool,
    L: FnMut(&RegistryEntry) -> bool,
    B: FnMut(&RegistryEntry) -> bool,
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
                // Recovery needs BOTH signals. A store hit alone means "the
                // session still exists" (= resumable), which for a store that
                // never evicts is permanently true - opencode's session table
                // keeps a row forever, so a dead pane would be resurrected to
                // `live` on every sweep and discovery would hand out a
                // recipient nobody drains. A row with no recorded pid keeps the
                // old behavior (`pid_live` is true), so exec rows are untouched.
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
fn late_bind_codex_sessions(
    home: &AgentsHome,
    emitter: &EventEmitter,
    probe: &dyn Fn(u32) -> Option<String>,
) -> Result<(), String> {
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();
    let candidates: Vec<(String, u32)> = registry
        .entries
        .iter()
        .filter(|e| {
            e.harness_name() == "codex"
                && e.mux.is_some()
                && e.harness_session_id.is_none()
                && e.pid.is_some_and(|p| pid_is_ours(p, e.pid_start_time))
        })
        .filter_map(|e| e.pid.map(|p| (e.name.clone(), p)))
        .collect();
    // A collision on one candidate must not starve the rest: every candidate
    // in this tick gets attempted, and the first write failure is what's
    // returned (code-review finding on this commit) -- returning early on the
    // first `Err` left a persistently-colliding row at the front of the scan
    // starving every sibling candidate's bind, forever, since candidates are
    // rescanned in the same registry order on every subsequent sweep.
    let mut first_error: Option<String> = None;
    for (name, pid) in candidates {
        let Some(sid) = probe(pid) else { continue };
        let bound = match state::update_registry(&home.registry_json(), |r| {
            let Some(e) = r.find_mut(&name) else {
                return false;
            };
            // A concurrent writer may have bound this row (or reaped it) since
            // the candidate scan above; never clobber a session id that
            // arrived in between.
            if e.harness_session_id.is_some() {
                return false;
            }
            e.harness_session_id = Some(sid.clone());
            true
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
            let _ = emitter.emit_fields(
                "agent_late_bind",
                json_obj(&[
                    ("name", Value::String(name)),
                    ("pid", Value::Number(pid.into())),
                    ("harness_session_id", Value::String(sid)),
                ]),
            );
        }
    }
    match first_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
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

fn run_reconcile_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
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
    let (changes, outcome) = plan_reconcile(
        &entries,
        probe,
        || start.elapsed() >= RECONCILE_SWEEP_BUDGET,
        pid_live,
        bg_live,
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
    // Deliver via the Python sidecar (`fno mcp send`), inheriting its lazy-start
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

/// Shell `fno mcp send --session <id>` with `envelope` on stdin (never argv - it
/// can be large). Returns `Err(reason)` on any failure (spawn or non-zero exit),
/// with the stderr tail as the reason.
fn deliver_envelope(channel_id: &str, envelope: &Value) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = crate::loop_dispatch::fno_cmd("fno")
        .args(["mcp", "send", "--session-id", channel_id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `fno mcp send` failed: {e}"))?;
    // Write + close stdin (drop => EOF) so the child's `stdin.read()` completes.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child stdin unavailable".to_string())?;
        let bytes = serde_json::to_vec(envelope).map_err(|e| format!("serialize envelope: {e}"))?;
        stdin
            .write_all(&bytes)
            .map_err(|e| format!("write envelope to `fno mcp send`: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait for `fno mcp send`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail = stderr.trim().rsplit('\n').next().unwrap_or("").trim();
    Err(if tail.is_empty() {
        format!("`fno mcp send` exited {}", out.status)
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
    use super::*;

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

    // One-shot ask row (empty short_id + no pid): terminal, reapable on grace
    // alone (owns no worktree). `exited_at` controls the grace clock.
    fn ask_row(name: &str, exited_at: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            legacy_provider: "claude".into(),
            provider: None,
            model: None,
            effort: None,
            harness: None,
            // x-7bcd: needs a resolvable handle (leg 3); deterministic per
            // name so two rows never collide.
            harness_session_id: Some(format!("{name}-sess")),
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
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
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

        let response = handle_rm_with(&ctx, &request, &snapshots, &|_| Ok(()), &|_, _| {
            Err("permission denied".into())
        })
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
    async fn rm_reports_when_an_oversized_event_is_replaced() {
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
        )
        .await;

        assert_eq!(response.result().unwrap()["event_written"], false);
        assert!(response.result().unwrap()["event_reason"]
            .as_str()
            .unwrap()
            .contains("event_payload_too_large"));
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
        )
        .await;

        assert!(response.error().is_some());
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
        )
        .await;

        let message = &response.error().unwrap().message;
        assert!(message.contains("claude agents --json --all"));
        assert!(message.contains("claude stop bbbb8888"));
        assert!(message.contains("claude rm bbbb8888"));
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

    #[test]
    fn mux_missing_pane_receipt_is_idempotent_absence() {
        assert!(mux_pane_is_absent("fno mux: no such pane: 24"));
        assert!(mux_pane_is_absent(
            "cannot reach session main: No such file or directory (os error 2)"
        ));
        assert!(!mux_pane_is_absent("mux configuration not found"));
        assert!(!mux_pane_is_absent("fno mux: permission denied"));
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

        let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600));

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
            &|_| Some(quiet.to_string()),
        );

        assert_eq!(swept, 2, "a tick that finds nothing must still report");
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert_eq!(log.matches("worktree_sweep").count(), 2);
        assert!(log.contains("report-only"));
    }

    #[test]
    fn sweep_honours_its_own_24h_floor() {
        let home = tmp_home("wt-sweep-floor");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let out = |_: &str| Some(REAL_SUMMARY.to_string());
        let now = 1_000_000;

        assert_eq!(
            worktree_sweep(&home, &emitter, now, &["/repo/a".into()], &out),
            1
        );
        // Same day: skipped entirely, no second reading.
        assert_eq!(
            worktree_sweep(&home, &emitter, now + 60, &["/repo/a".into()], &out),
            0
        );
        // A day later: fires again.
        assert_eq!(
            worktree_sweep(&home, &emitter, now + 86_401, &["/repo/a".into()], &out),
            1
        );
    }

    #[test]
    fn sweep_never_passes_apply() {
        // Ruling: a merged PR is proof, a timer tick is not. Removal lives on the
        // merge-triggered path only. Pin that this sweep cannot grow an --apply.
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

        let swept = worktree_sweep(&home, &emitter, 1_000_000, &["/repo/a".into()], &|_| None);

        assert_eq!(swept, 0);
        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(log.contains("unreadable-summary"));
        assert!(!log.contains("\"eligible\""));
    }

    #[test]
    fn transcript_probe_reads_freshness_and_fails_to_unknown() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let now = now_epoch_secs();

        // No path recorded, and a path that does not exist: UNKNOWN, never a
        // cheerful "stale". The caller reads None as "keep".
        assert_eq!(transcript_fresh_probe(None, now, 3600), None);
        assert_eq!(transcript_fresh_probe(Some(""), now, 3600), None);
        assert_eq!(
            transcript_fresh_probe(Some("/nonexistent/transcript.jsonl"), now, 3600),
            None
        );

        let fresh = dir.path().join("fresh.jsonl");
        std::fs::write(&fresh, "{}\n").unwrap();
        assert_eq!(
            transcript_fresh_probe(Some(&fresh.to_string_lossy()), now, 3600),
            Some(true),
            "a just-written transcript is a worker that is still around"
        );

        let stale = dir.path().join("stale.jsonl");
        std::fs::write(&stale, "{}\n").unwrap();
        assert!(std::process::Command::new("touch")
            .args(["-t", "200001010000", &stale.to_string_lossy()])
            .status()
            .expect("touch runs")
            .success());
        assert_eq!(
            transcript_fresh_probe(Some(&stale.to_string_lossy()), now, 3600),
            Some(false),
            "a transcript untouched for the window is a session that stopped"
        );
    }

    #[test]
    fn the_exit_stamp_is_written_per_sweep_not_per_exit() {
        // NAMING THE BATCH WRITER. `gc_sweep` is the only production writer of
        // `exited_at`, and it computes ONE timestamp per pass and applies it to
        // every row it newly observes as dead. That is why rows across unrelated
        // tenants and projects share a stamp to the second: the field measures a
        // sweep tick, not an exit.
        //
        // This test pins the shape so the field cannot quietly start looking like
        // real exit evidence and get trusted on its own again.
        let home = tmp_home("gc-batch-stamp");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            for n in ["ask-a", "ask-b", "ask-c"] {
                r.entries.push(ask_row(n, None));
            }
        })
        .unwrap();

        gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600));

        let reg = state::load_registry(&home.registry_json()).unwrap();
        let stamps: std::collections::BTreeSet<String> = reg
            .entries
            .iter()
            .filter_map(|e| e.exited_at.clone())
            .collect();
        assert_eq!(reg.entries.len(), 3, "nothing reaped on the stamping pass");
        assert_eq!(
            stamps.len(),
            1,
            "three unrelated rows share one stamp: it is a sweep tick, not an exit"
        );
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
        let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600));
        assert!(summary.reaped.is_empty());
        assert!(summary.kept_dirty.is_empty());
        assert!(summary.kept_uncorroborated.is_empty());
    }

    /// x-9de7 task 5: the "stuck and invisible" case named in the plan -
    /// past grace, a liveness surface on record, but no positive corroboration
    /// (no confirmed-dead pid, no resolvable transcript) and short of the
    /// backstop horizon. Before `kept_uncorroborated` this row was neither
    /// reaped nor reported: an operator staring at `fno agents reap` saw
    /// nothing at all.
    #[test]
    fn gc_sweep_reports_kept_uncorroborated_for_the_stuck_and_invisible_row() {
        let home = tmp_home("gc-uncorroborated");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        // Past a 1h grace, nowhere near the 7-day backstop horizon: the case
        // the corroboration gate exists to hold, not the escape hatch.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
        let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = ask_row("stuck", Some(exited_at.as_str()));
            e.short_id = "stuck".into(); // liveness_surface, no live socket
            e.log_path = None; // transcript unresolvable -> transcript_fresh: None
            r.entries.push(e);
        })
        .unwrap();

        // gc_sweep_impl directly, not the gc_sweep wrapper: the wrapper's
        // HarnessStoreIndex::default() falls back to the REAL $HOME when a
        // developer machine has one, so "stuck"'s synthetic session id can
        // resolve against the real ~/.claude/projects and read as gone -
        // false corroboration this test exists to rule out. Stub `None`
        // (unresolvable) is the harness-store answer this test is about.
        let summary = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            false,
            &|_| None,
            &|_| None,
            &|_| None,
        );

        assert!(
            summary.reaped.is_empty(),
            "no corroboration -> never reaped"
        );
        assert!(summary.kept_dirty.is_empty(), "not a worktree case");
        assert_eq!(summary.kept_uncorroborated, vec!["stuck".to_string()]);

        // The row itself is untouched (still on disk, unstamped-differently).
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.entries.iter().any(|e| e.name == "stuck"));
    }

    // -- The harness-store corroboration seam (AC1 / AC3 / AC5) -------------

    #[test]
    fn harness_store_keying_never_judges_one_harness_by_anothers_store() {
        // THE AC3 SPECIMEN: a codex worker has no claude transcript by
        // construction. Judged by claude's store it reads "session gone" and
        // reaps; judged by its own (empty) codex store it also reads gone -
        // but the keying is what makes each answer belong to the right row,
        // and an unknown harness must answer None (unjudgeable), never borrow
        // either store.
        let dir = tempfile::tempdir().expect("tmpdir");
        let claude_root = dir.path().join("claude-projects");
        let codex_root = dir.path().join("codex-sessions");
        let project = claude_root.join("-proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sid-1234.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(&codex_root).unwrap();

        let row_for = |harness: Option<&str>, sid: Option<&str>| RegistryEntry {
            harness: harness.map(str::to_string),
            harness_session_id: sid.map(str::to_string),
            ..ask_row("keying", None)
        };
        let mut idx = HarnessStoreIndex::with_roots(claude_root.clone(), codex_root.clone());

        let claude = idx
            .matches(&row_for(Some("claude"), Some("sid-1234")))
            .expect("claude store is readable, so it answers");
        assert_eq!(claude.len(), 1, "the session exists in claude's own store");

        let codex = idx
            .matches(&row_for(Some("codex"), Some("sid-1234")))
            .expect("codex store is readable, so it answers");
        assert!(
            codex.is_empty(),
            "the codex store holds no such session: its own answer, independent of claude's"
        );

        for unknown in ["gemini", "opencode", "agy"] {
            assert!(
                idx.matches(&row_for(Some(unknown), Some("sid-1234")))
                    .is_none(),
                "an unknown harness ({unknown}) is unjudgeable, never judged by another store"
            );
        }
        // A row with no resolvable identity at all -- harness AND the legacy
        // provider fallback both blank -- is unjudgeable too. Distinct from
        // `harness: Some("")` alone: harness_name() treats a blank `harness`
        // the same as `None` and falls back to legacy_provider (tested below,
        // where the fixture's legacy_provider is "claude"), so this case must
        // blank the fallback too or it silently resolves to a known store.
        let blank = RegistryEntry {
            harness: Some(String::new()),
            harness_session_id: Some("sid-1234".to_string()),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            effort: None,
            ..ask_row("keying", None)
        };
        assert!(
            idx.matches(&blank).is_none(),
            "no resolvable harness identity at all is unjudgeable"
        );
        // No session id on the row: unjudgeable even for a known harness.
        assert!(idx.matches(&row_for(Some("claude"), None)).is_none());
        // A row whose harness falls back to the legacy provider field keys the
        // same way (harness_name resolves the alias).
        let mut legacy = HarnessStoreIndex::with_roots(claude_root, codex_root);
        let hits = legacy
            .matches(&row_for(None, Some("sid-1234")))
            .expect("claude (via legacy provider) store is readable, so it answers");
        assert_eq!(hits.len(), 1);
    }

    /// AC1 end-to-end: an exited row past grace whose harness session is gone
    /// from its own store reaps, with the event carrying harness + session id
    /// (the resumable/diagnostic handle), and the registry reap is never
    /// blocked by a cascade that finds nothing to remove.
    #[test]
    fn gc_sweep_reaps_on_a_gone_harness_session_and_records_the_handle() {
        let home = tmp_home("gc-harness-gone");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
        let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = ask_row("gone-session", Some(exited_at.as_str()));
            e.short_id = "gonesess".into(); // liveness surface, no live socket
            e.log_path = None; // the dead-evidence file that no longer exists
            e.harness = Some("claude".into());
            e.harness_session_id = Some("80e70ab4-1111".into());
            r.entries.push(e);
        })
        .unwrap();

        let summary = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            false,
            &|_| None, // truth tail: no live rows to probe
            // Session gone from its own store: the empty hit vector.
            &|e| (e.name == "gone-session").then(Vec::new),
            &|_| None, // cascade: store holds nothing to remove
        );

        assert_eq!(summary.reaped, vec!["gonesess".to_string()]);
        assert!(summary.cascade_refused.is_empty());
        // POSITIVE MARKER: the row is absent from the registry after the call,
        // not merely absent from the error output.
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(
            reg.entries.iter().all(|e| e.name != "gone-session"),
            "the reap must be observable in the registry itself"
        );
        // The event carries the resumable handle fields.
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(
            events.contains("\"harness_session_id\"") && events.contains("80e70ab4-1111"),
            "agent_row_reaped must record the harness session handle"
        );
    }

    /// AC4 end-to-end: an Orphaned row earns an exit stamp on first sight and
    /// reaps once past grace with corroboration - the immortal-row mechanism.
    #[test]
    fn gc_sweep_orphaned_row_stamps_then_reaps() {
        let home = tmp_home("gc-orphaned");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
        let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");

        // First pass: unstamped Orphaned -> StampExit, nothing removed.
        state::update_registry(&home.registry_json(), |r| {
            let mut e = ask_row("orph", None);
            e.status = AgentStatus::Orphaned;
            e.short_id = "orph".into();
            e.log_path = None;
            r.entries.push(e);
        })
        .unwrap();
        let first = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            false,
            &|_| None,
            &|_| None,
            &|_| None,
        );
        assert!(first.reaped.is_empty(), "first sight stamps, never reaps");
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let stamped = reg
            .entries
            .iter()
            .find(|e| e.name == "orph")
            .expect("row survives the stamping pass")
            .exited_at
            .clone();
        assert!(
            stamped.is_some(),
            "an unreachable probe is an observation: the clock starts"
        );

        // Backdate past grace, corroborate, sweep again: reaped.
        state::update_registry(&home.registry_json(), |r| {
            for e in r.entries.iter_mut() {
                if e.name == "orph" {
                    e.exited_at = Some(exited_at.clone());
                }
            }
        })
        .unwrap();
        let second = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            false,
            &|_| None,
            &|e| (e.name == "orph").then(Vec::new),
            &|_| None,
        );
        assert_eq!(second.reaped, vec!["orph".to_string()]);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.entries.iter().all(|e| e.name != "orph"));
    }

    /// AC7 end-to-end: a LIVE row idle past grace whose tail reads done leaves
    /// as a dormant reap (resumable: true in the event); one whose tail reads
    /// anything else stays. The credential-dead worker is the second case.
    #[test]
    fn gc_sweep_live_done_row_leaves_as_dormant_and_other_tails_stay() {
        let home = tmp_home("gc-dormant");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
        let idle_since = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        // Live rows: our own pid, bare-existence live (the fixture pattern the
        // keeps_live test uses), idle two hours against a 1h grace.
        state::update_registry(&home.registry_json(), |r| {
            let mut done = ask_row("bg-done", None);
            done.status = AgentStatus::Live;
            done.short_id = "bgdone".into();
            done.last_message_at = Some(idle_since.clone());
            done.pid = Some(std::process::id());
            r.entries.push(done);
            let mut watching = ask_row("bg-watch", None);
            watching.status = AgentStatus::Live;
            watching.short_id = "bgwatch".into();
            watching.last_message_at = Some(idle_since.clone());
            watching.pid = Some(std::process::id());
            r.entries.push(watching);
        })
        .unwrap();

        let summary = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            false,
            &|handle| match handle {
                "bgdone" => Some("done".to_string()),
                _ => Some("watching".to_string()), // the credential-dead shape
            },
            &|_| None,
            &|_| None,
        );

        assert_eq!(summary.reaped_dormant, vec!["bgdone".to_string()]);
        assert!(summary.reaped.is_empty(), "a finished turn is not a death");
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(
            events.contains("\"resumable\":true"),
            "the dormant reap must record resumability for the dormant distinction"
        );
        // POSITIVE MARKER: only the done row left the registry.
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.entries.iter().all(|e| e.name != "bg-done"));
        assert!(
            reg.entries.iter().any(|e| e.name == "bg-watch"),
            "a live row whose tail is not done stays, whatever its idle age"
        );
    }

    /// AC6: a reaped row whose harness session refuses removal keeps the
    /// refusal in the report (surfaced, never swallowed), and the registry reap
    /// is NOT rolled back. Cascade injected so the staged refusal is
    /// deterministic - no PATH/HOME games against a parallel test run.
    #[test]
    fn gc_sweep_cascade_refusal_is_surfaced_and_never_undoes_the_reap() {
        let home = tmp_home("gc-cascade");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
        let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = ask_row("pid-dead", Some(exited_at.as_str()));
            e.short_id = "piddead".into();
            e.log_path = None;
            e.harness = Some("codex".into());
            e.harness_session_id = Some("sid-cascade".into());
            e.pid = Some(999_999_999); // no such process: confirmed dead
            r.entries.push(e);
        })
        .unwrap();

        let summary = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            false,
            &|_| None,
            &|_| None,
            // The cascade refuses for this row: the harness store would not
            // give the session up.
            &|e| {
                (e.name == "pid-dead").then(|| {
                    (
                        "piddead".to_string(),
                        "cascade refused (staged)".to_string(),
                    )
                })
            },
        );

        assert_eq!(summary.reaped, vec!["piddead".to_string()]);
        assert_eq!(
            summary.cascade_refused,
            vec![(
                "piddead".to_string(),
                "cascade refused (staged)".to_string()
            )],
            "the refusal is surfaced with its reason"
        );
        // POSITIVE MARKER: the registry itself no longer holds the row - the
        // refusal never rolled back the reap.
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.entries.iter().all(|e| e.name != "pid-dead"));
    }

    /// The codex cascade core: index surgery drops only the matching session's
    /// entry, keeps unparsable lines, and reports nothing to remove as a no-op.
    #[test]
    fn codex_index_cascade_drops_only_the_matching_entry() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let index = dir.path().join("session_index.jsonl");
        std::fs::write(
            &index,
            concat!(
                "{\"session_id\":\"aaa\"}\n",
                "not-json-at-all\n",
                "{\"session_id\":\"bbb\"}\n",
            ),
        )
        .unwrap();
        assert_eq!(cascade_codex_index(&index, "missing", "row"), Ok(false));
        let after_noop = std::fs::read_to_string(&index).unwrap();
        assert_eq!(
            after_noop.lines().count(),
            3,
            "no match: byte-for-byte no-op"
        );

        assert_eq!(cascade_codex_index(&index, "aaa", "row"), Ok(true));
        let after = std::fs::read_to_string(&index).unwrap();
        assert!(!after.contains("\"aaa\""), "the matching entry is gone");
        assert!(after.contains("\"bbb\""), "other entries stay");
        assert!(
            after.contains("not-json-at-all"),
            "an unparsable line is never destroyed"
        );

        // A missing index is a no-op, not a refusal.
        assert_eq!(
            cascade_codex_index(&dir.path().join("nope.jsonl"), "aaa", "row"),
            Ok(false)
        );
    }

    /// `--dry-run` (x-9de7 task 5): the same classification as a real sweep,
    /// including the kept-reason diagnostics, but the registry is provably
    /// untouched and no `agent_row_reaped` event lands.
    #[test]
    fn gc_sweep_dry_run_reports_without_mutating() {
        let home = tmp_home("gc-dry-run");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
        let recent_exit = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        state::update_registry(&home.registry_json(), |r| {
            // Would reap (no liveness surface -> auto-corroborated regardless
            // of age, so an old stamp is fine here).
            r.entries
                .push(ask_row("ask-old", Some("2020-01-01T00:00:00Z")));
            // Would keep uncorroborated: past grace, short of the 7-day
            // backstop horizon.
            let mut stuck = ask_row("stuck", Some(recent_exit.as_str()));
            stuck.short_id = "stuck".into();
            stuck.log_path = None;
            r.entries.push(stuck);
        })
        .unwrap();
        let before = state::load_registry(&home.registry_json()).unwrap();

        // gc_sweep_impl directly, not the gc_sweep_dry_run wrapper: see the
        // comment on gc_sweep_reports_kept_uncorroborated_for_the_stuck_and_invisible_row
        // for why the wrapper's real-$HOME HarnessStoreIndex is not hermetic.
        let emitter = EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let summary = gc_sweep_impl(
            &home,
            &emitter,
            &|_| Duration::from_secs(3600),
            true,
            &|_| None,
            &|_| None,
            &|_| None,
        );

        assert_eq!(summary.reaped, vec!["ask-old".to_string()]);
        assert_eq!(summary.kept_uncorroborated, vec!["stuck".to_string()]);

        let after = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(
            before.entries.len(),
            after.entries.len(),
            "dry-run must not remove a row"
        );
        assert!(
            after.entries.iter().any(|e| e.name == "ask-old"),
            "dry-run must not remove ask-old from disk"
        );
        assert!(
            !std::path::Path::new(&home.events_jsonl()).exists(),
            "dry-run must never emit agent_row_reaped"
        );
    }

    /// The long-silence repro (x-9de7 verification #7, the one task 6 exists
    /// for). A codex row whose transcript went untouched for 90 minutes while
    /// the pane was alive: under the OLD one-hour-for-every-harness window
    /// that silence corroborates a reap; under an 8h codex grace it does not.
    /// Both sweeps run against the SAME fixture (same exited_at, same
    /// transcript mtime) so the only variable is the grace the resolver hands
    /// back for "codex".
    #[test]
    fn gc_sweep_a_90_minute_codex_silence_reaps_under_1h_grace_not_under_8h() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let exited_at_secs = now - 9 * 3600; // well past either grace
        let (y, mo, d, h, mi, s) = civil(exited_at_secs);
        let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");

        let seed = |tag: &str| -> (AgentsHome, std::path::PathBuf) {
            let home = tmp_home(tag);
            let log_path = home.root().join("transcript.jsonl");
            std::fs::write(&log_path, "{}\n").unwrap();
            let mtime =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(now - 90 * 60);
            std::fs::File::options()
                .write(true)
                .open(&log_path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            state::update_registry(&home.registry_json(), |r| {
                let mut e = ask_row("codex-silent", Some(exited_at.as_str()));
                e.short_id = "codex-silent".into(); // liveness_surface, no live socket
                e.harness = Some("codex".into());
                e.log_path = Some(log_path.to_string_lossy().into_owned());
                r.entries.push(e);
            })
            .unwrap();
            (home, log_path)
        };

        // gc_sweep_impl directly, not the gc_sweep wrapper: see the comment
        // on gc_sweep_reports_kept_uncorroborated_for_the_stuck_and_invisible_row
        // for why the wrapper's real-$HOME HarnessStoreIndex is not hermetic
        // - here it would auto-corroborate via harness_session_gone
        // regardless of the transcript-mtime freshness this test is about.

        // OLD behaviour: one number for every harness.
        let (home_old, _) = seed("gc-silence-old");
        let emitter_old = EventEmitter::new(home_old.events_jsonl(), "daemon");
        let summary_old = gc_sweep_impl(
            &home_old,
            &emitter_old,
            &|_| Duration::from_secs(3600),
            false,
            &|_| None,
            &|_| None,
            &|_| None,
        );
        assert_eq!(
            summary_old.reaped,
            vec!["codex-silent".to_string()],
            "control: a 1h window reads 90 minutes of silence as corroborated staleness"
        );

        // FIXED behaviour: codex gets its own 8h grace/freshness window.
        let (home_new, _) = seed("gc-silence-new");
        let emitter_new = EventEmitter::new(home_new.events_jsonl(), "daemon");
        let summary_new = gc_sweep_impl(
            &home_new,
            &emitter_new,
            &|harness| Duration::from_secs(if harness == "codex" { 8 * 3600 } else { 3600 }),
            false,
            &|_| None,
            &|_| None,
            &|_| None,
        );
        assert!(
            summary_new.reaped.is_empty(),
            "an 8h codex grace must not corroborate a worker that was silent for only 90 minutes"
        );
    }

    #[test]
    fn gc_sweep_turns_unterminated_node_reap_into_durable_failure() {
        let sandbox = tmp_home("gc-dead-dispatch");
        let home = AgentsHome::at(sandbox.root().join("agents"));
        home.ensure_root().unwrap();
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let dead_repo = home.root().join("dead-repo");
        let done_repo = home.root().join("done-repo");
        for repo in [&dead_repo, &done_repo] {
            std::fs::create_dir_all(repo.join(".fno")).unwrap();
            assert!(std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo)
                .status()
                .unwrap()
                .success());
        }

        let dead_session = "target-run-dead";
        let done_session = "target-run-done";
        std::fs::write(
            dead_repo.join(".fno/target-state.md"),
            format!("---\nfno_id: {dead_session}\ninput: x-a35a\nplan_path: \"\"\n---\n"),
        )
        .unwrap();
        std::fs::write(
            done_repo.join(".fno/target-state.md"),
            format!("---\nfno_id: {done_session}\ninput: x-b44e\nplan_path: \"\"\n---\n"),
        )
        .unwrap();
        state::update_registry(&home.registry_json(), |r| {
            let mut dead = bg_claude_row("target-x-a35a-route-atomicity", "dead0001");
            dead.status = AgentStatus::Exited;
            dead.cwd = dead_repo.to_string_lossy().into_owned();
            dead.exited_at = Some("2020-01-01T00:00:00Z".into());
            dead.log_path = Some(stale_log(&dead_repo));
            dead.harness_session_id = Some("dead-harness-uuid".into());
            r.entries.push(dead);

            let mut done = bg_claude_row("target-x-b44e-finished", "done0002");
            done.status = AgentStatus::Exited;
            done.cwd = done_repo.to_string_lossy().into_owned();
            done.exited_at = Some("2020-01-01T00:00:00Z".into());
            done.log_path = Some(stale_log(&done_repo));
            done.harness_session_id = Some("done-harness-uuid".into());
            r.entries.push(done);
        })
        .unwrap();

        let global_events = home.root().parent().unwrap().join("events.jsonl");
        std::fs::write(
            done_repo.join(".fno/events.jsonl.1"),
            format!(
                "{{\"ts\":\"2026-07-24T00:00:00Z\",\"type\":\"termination\",\"source\":\"loop\",\"data\":{{\"session_id\":\"{done_session}\",\"reason\":\"DonePRGreen\",\"message\":\"done\"}}}}\n"
            ),
        )
        .unwrap();

        let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(0));
        assert_eq!(summary.reaped.len(), 2);

        let reaps = read_events(&home);
        let dead_reap = reaps
            .iter()
            .find(|e| e["data"]["short_id"] == "dead0001")
            .expect("dead dispatch reap event");
        assert_eq!(dead_reap["data"]["node_id"], "x-a35a");
        assert_eq!(dead_reap["data"]["termination_event"], false);
        let done_reap = reaps
            .iter()
            .find(|e| e["data"]["short_id"] == "done0002")
            .expect("completed dispatch reap event");
        assert_eq!(done_reap["data"]["node_id"], "x-b44e");
        assert_eq!(done_reap["data"]["termination_event"], true);

        let global = std::fs::read_to_string(&global_events).unwrap();
        let failures: Vec<Value> = global
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|e: &Value| e["type"] == "node_failed")
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["data"]["unit_id"], "x-a35a");
        assert_eq!(failures[0]["data"]["session_id"], dead_session);
        assert_eq!(
            failures[0]["data"]["reason"],
            "agent-row-reaped-no-termination"
        );
    }

    #[test]
    fn gc_sweep_restores_row_when_termination_evidence_is_unknown() {
        let sandbox = tmp_home("gc-unknown-termination");
        let home = AgentsHome::at(sandbox.root().join("agents"));
        home.ensure_root().unwrap();
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let repo = home.root().join("repo");
        std::fs::create_dir_all(repo.join(".fno")).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(
            repo.join(".fno/target-state.md"),
            "---\nfno_id: reused-run\ninput: x-other\nplan_path: \"\"\n---\n",
        )
        .unwrap();
        state::update_registry(&home.registry_json(), |registry| {
            let mut row = bg_claude_row("target-x-a35a-route-atomicity", "dead0001");
            row.status = AgentStatus::Exited;
            row.cwd = repo.to_string_lossy().into_owned();
            row.exited_at = Some("2020-01-01T00:00:00Z".into());
            row.log_path = Some(stale_log(&repo));
            registry.entries.push(row);
        })
        .unwrap();

        let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(0));

        assert!(summary.reaped.is_empty());
        let registry = state::load_registry(&home.registry_json()).unwrap();
        assert!(registry
            .entries
            .iter()
            .any(|row| row.name == "target-x-a35a-route-atomicity"));
        let events = read_events(&home);
        assert!(events.iter().any(|event| {
            event["type"] == "daemon_recovery_error"
                && event["data"]["op"] == "observe_dead_dispatch_termination"
        }));
    }

    #[test]
    fn gc_sweep_restores_row_when_dead_dispatch_receipt_cannot_persist() {
        let sandbox = tmp_home("gc-dead-dispatch-write-failure");
        let home = AgentsHome::at(sandbox.root().join("agents"));
        home.ensure_root().unwrap();
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let repo = home.root().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        state::update_registry(&home.registry_json(), |registry| {
            let mut row = bg_claude_row("target-x-a35a-route-atomicity", "dead0001");
            row.status = AgentStatus::Exited;
            row.cwd = repo.to_string_lossy().into_owned();
            row.exited_at = Some("2020-01-01T00:00:00Z".into());
            row.log_path = Some(stale_log(&repo));
            registry.entries.push(row);
        })
        .unwrap();
        std::fs::create_dir_all(global_events_path(&home)).unwrap();

        let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(0));

        assert!(summary.reaped.is_empty());
        let registry = state::load_registry(&home.registry_json()).unwrap();
        assert!(registry
            .entries
            .iter()
            .any(|row| row.name == "target-x-a35a-route-atomicity"));
        let events = read_events(&home);
        assert!(events.iter().any(|event| {
            event["type"] == "daemon_recovery_error"
                && event["data"]["op"] == "record_dead_dispatch"
        }));
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
                legacy_provider: "codex".into(),
                provider: None,
                model: None,
                effort: None,
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
                log_path: Some("/tmp/worker-A.log".into()), // x-7bcd: resolvable handle
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

        let report = recover(&home, &emitter).expect("startup recovery");
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
                legacy_provider: "codex".into(),
                provider: None,
                model: None,
                effort: None,
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
                log_path: Some("/tmp/ghost.log".into()), // x-7bcd: resolvable handle
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
            });
        })
        .unwrap();
        // No state.json written for "ghost".
        let report = recover(&home, &emitter).expect("startup recovery");
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
        let report = recover(&home, &emitter).expect("startup recovery");
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

    #[tokio::test]
    async fn lifecycle_name_resolution_never_falls_back_on_ambiguity() {
        let mut named = rentry("deadbeef", AgentStatus::Live, None);
        named.short_id = "transport-a".into();
        named.harness_session_id = Some("aaaaaaaa-1111-2222-3333-444455556666".into());
        let mut short = rentry("other", AgentStatus::Live, None);
        short.short_id = "deadbeef".into();
        short.harness_session_id = Some("bbbbbbbb-1111-2222-3333-000000000002".into());
        let reg = crate::state::Registry {
            schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
            entries: vec![named, short],
        };

        let error = entry_for_lifecycle(
            &reg,
            "deadbeef",
            std::path::Path::new("/nonexistent/registry.json"),
        )
        .await
        .expect_err("ambiguous token must not fall back to the matching row name");

        assert!(error.contains("ambiguous across 2 agents"));
    }

    #[test]
    fn recovery_reaps_dead_pid() {
        let home = tmp_home("recover-reap");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                name: "dead".into(),
                short_id: "dead".into(),
                legacy_provider: "codex".into(),
                provider: None,
                model: None,
                effort: None,
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
                log_path: Some("/tmp/dead.log".into()), // x-7bcd: resolvable handle
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
            });
        })
        .unwrap();
        // Give it a state.json so it isn't flagged inconsistent.
        let mut st = AgentState::new_pty("dead");
        st.status = AgentStatus::Live;
        state::write_state_atomic(&home.state_json("dead"), &st).unwrap();

        let report = recover(&home, &emitter).expect("startup recovery");
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
            e.log_path = Some("/tmp/hosted.log".into()); // x-7bcd: resolvable handle
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
    fn recovery_orphan_pid_sweep_does_not_condemn_every_row_sharing_an_empty_short_id() {
        // x-9de7 task 1: the orphan-PID sweep (Step 6 of recover(), ~line 270)
        // collects reaped short_ids into a `BTreeSet<String>`, then marks EVERY
        // entry whose short_id is a MEMBER of that set as Exited -- not just the
        // specific entry that failed pid_is_ours. Every codex/gemini shellout
        // row shares the same empty short_id (see the comment at the top of
        // recover()), so one genuinely dead pane-hosted row poisons every live
        // one that happens to sit beside it in the registry. This is the writer
        // behind the false `exited` write on a live mux pane row.
        let home = tmp_home("recover-empty-short-id-collision");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        let Some(my_start) = process_start_time(me) else {
            return; // platform without start-time support; nothing to assert
        };
        state::update_registry(&home.registry_json(), |r| {
            // Genuinely dead: pid_is_ours must return false for this one.
            let mut dead = ask_row("dead-pane", None);
            dead.status = AgentStatus::Live;
            dead.pid = Some(0x7fff_fff0); // not a live process
            r.entries.push(dead);

            // Live: real pid, matching start time, hosted in a mux pane -- same
            // empty short_id as the dead row above.
            let mut live = ask_row("live-pane", None);
            live.status = AgentStatus::Live;
            live.pid = Some(me);
            live.pid_start_time = Some(my_start);
            live.mux = Some(state::MuxRef {
                session: "main".into(),
                pane_id: 1,
            });
            r.entries.push(live);
        })
        .unwrap();

        let _ = recover(&home, &emitter);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let live = reg.find("live-pane").unwrap();
        assert_eq!(
            live.status,
            AgentStatus::Live,
            "a live pane-hosted row must not be condemned by a sibling's empty short_id"
        );
        assert!(
            live.pid.is_some(),
            "the writer clears no pid; a fix must not start clearing it here either"
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

    // ---- x-cd31: idle-exit reads live workers, not registry emptiness ------

    /// A row with a live-pid shape (short_id set, pid + matching start time),
    /// the row that must PIN the daemon.
    fn live_pid_row(short_id: &str) -> RegistryEntry {
        let mut row = ask_row(short_id, None);
        row.short_id = short_id.to_string();
        row.pid = Some(std::process::id());
        row.pid_start_time = process_start_time(std::process::id());
        row
    }

    #[test]
    fn idle_exit_fires_on_terminal_rows_with_no_live_worker() {
        // The exact defect box: a registry of TERMINAL rows (the roster an
        // established machine always has) with no live socket and no live pid
        // must let the daemon exit. Registry emptiness never held here.
        let home = short_home("idle-terminal");
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries
                .push(ask_row("done-1", Some("2020-01-01T00:00:00Z")));
            r.entries
                .push(ask_row("done-2", Some("2020-01-01T00:00:00Z")));
        })
        .unwrap();
        assert!(
            no_live_worker(&home),
            "terminal rows with dead pids and no sockets must not pin the daemon"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn idle_exit_held_by_a_live_worker_socket() {
        let home = short_home("idle-sock");
        home.ensure_root().unwrap();
        std::fs::create_dir_all(home.agent_dir("wka")).unwrap();
        // A REAL listener: since the stale-socket fix, file existence alone
        // does not pin the daemon - something must answer on the socket.
        let _listener = std::os::unix::net::UnixListener::bind(home.worker_sock("wka")).unwrap();
        state::update_registry(&home.registry_json(), |r| {
            // Terminal row, dead pid, but a live worker serving on its socket.
            let mut row = ask_row("wka", Some("2020-01-01T00:00:00Z"));
            row.short_id = "wka".to_string();
            r.entries.push(row);
        })
        .unwrap();
        assert!(
            !no_live_worker(&home),
            "a reachable worker socket pins the daemon regardless of what its row says"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn idle_exit_not_held_by_a_stale_socket_file() {
        // A worker killed without reaping its socket leaves the FILE behind.
        // Nothing answers on it, so it must not pin the daemon - the exact
        // stale-file case the connect probe exists for (a pid-less live row
        // would otherwise make the daemon immortal).
        let home = short_home("idle-stale");
        home.ensure_root().unwrap();
        std::fs::create_dir_all(home.agent_dir("wka")).unwrap();
        std::fs::write(home.worker_sock("wka"), b"").unwrap();
        state::update_registry(&home.registry_json(), |r| {
            let mut row = ask_row("wka", None);
            row.short_id = "wka".to_string();
            r.entries.push(row);
        })
        .unwrap();
        assert!(
            no_live_worker(&home),
            "a socket file nobody answers on is not a live worker"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn idle_exit_held_by_a_live_worker_pid() {
        let home = short_home("idle-pid");
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(live_pid_row("wkb"));
        })
        .unwrap();
        assert!(
            !no_live_worker(&home),
            "a row whose pid is still ours pins the daemon"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn idle_exit_held_by_an_unreadable_registry() {
        // The fail-safe side: an unreadable registry is an absence with two
        // explanations, and the daemon must stay resident rather than exit on
        // a transient read failure (the old code exited: unwrap_or(true)).
        let home = short_home("idle-unreadable");
        home.ensure_root().unwrap();
        std::fs::write(home.registry_json(), "not json at all{").unwrap();
        assert!(
            !no_live_worker(&home),
            "an unreadable registry must not license an idle exit"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn idle_exit_fires_on_a_fresh_home_with_no_registry() {
        // The first daemon on a fresh machine: no registry file has ever been
        // written, and lazy-exit must hold for it too (the missing file is
        // "nothing ever tracked", not a read failure).
        let home = short_home("idle-fresh");
        home.ensure_root().unwrap();
        assert!(
            no_live_worker(&home),
            "a fresh home with no registry must idle-exit"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn daemon_exited_payload_distinguishes_socket_loss() {
        // x-3498 AC: an abnormal retirement (socket path taken from under us)
        // must read differently from a graceful ending.
        assert_eq!(
            daemon_exited_payload("socket-lost"),
            json!({"clean": false, "reason": "socket-lost"})
        );
        assert_eq!(
            daemon_exited_payload("sigterm"),
            json!({"clean": true, "reason": "sigterm"})
        );
        assert_eq!(
            daemon_exited_payload("idle"),
            json!({"clean": true, "reason": "idle"})
        );
    }

    #[tokio::test]
    async fn stop_claude_pid_kills_a_real_child_and_spares_a_recycled_pid() {
        // x-a4b2: a row with a pid and no transport id must actually be stopped
        // (it used to be refused, leaving a live duplicate worker), and a pid
        // whose start time no longer matches must be left alone.
        let mut entry = ask_row("orphan", None);

        // A row with no pid at all has nothing to signal.
        assert!(
            !stop_claude_pid_confirmed(&entry).await,
            "no pid -> nothing to stop"
        );

        // Spawn the sleeper as a DETACHED grandchild: `sh` backgrounds it and
        // exits, so it is reparented away and is never this test's child. A
        // direct child would linger as a zombie after SIGTERM until reaped, and
        // `pid_is_ours` (a bare `kill(pid, 0)` probe) reads a zombie as alive.
        // The real claude worker is not the daemon's child either, so this also
        // matches production.
        let out = std::process::Command::new("sh")
            .arg("-c")
            // The redirect is load-bearing: a backgrounded child inherits sh's
            // stdout pipe, so without it `.output()` blocks for the full sleep
            // waiting on EOF instead of returning as soon as sh exits.
            .arg("sleep 60 >/dev/null 2>&1 & echo $!")
            .output()
            .expect("spawn detached sleeper");
        let pid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("sleeper pid");
        let start = process_start_time(pid);

        // Independent death oracle. Asserting with `pid_is_ours` would use the
        // subject's own probe as its judge, and that probe reports EPERM and a
        // recycled pid as not-ours too, so it can read "gone" over a process
        // that is still running. `ps` knows nothing about our guards.
        let ps_says_alive = |pid: u32| {
            std::process::Command::new("ps")
                .args(["-p", &pid.to_string()])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter(|l| l.split_whitespace().next() == Some(&pid.to_string()))
                        .count()
                        > 0
                })
                .unwrap_or(false)
        };

        // No incarnation token: bare liveness is not a licence to SIGKILL.
        entry.pid = Some(pid);
        entry.pid_start_time = None;
        assert!(
            !stop_claude_pid_confirmed(&entry).await,
            "no start token -> refuse"
        );
        assert!(ps_says_alive(pid), "a refused row must not be signalled");

        // Wrong incarnation token: the pid belongs to someone else now.
        if let Some(st) = start {
            entry.pid_start_time = Some(st.wrapping_add(1));
            assert!(
                !stop_claude_pid_confirmed(&entry).await,
                "recycled pid -> refuse"
            );
            assert!(
                ps_says_alive(pid),
                "an unrelated process must not be signalled"
            );
        }

        // Correct token: the process is really killed, not merely reported.
        entry.pid_start_time = start;
        if start.is_some() {
            assert!(
                stop_claude_pid_confirmed(&entry).await,
                "owned live pid -> stopped"
            );
            assert!(!ps_says_alive(pid), "process is gone");
        } else {
            // No readable start time on this platform: the guard above refuses
            // every row, so reap the sleeper rather than leaking it.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    #[test]
    fn pid_is_ours_rejects_an_out_of_range_pid() {
        // u32::MAX wraps to -1 in signed pid_t, the "signal every process I may
        // signal" broadcast target. kill(-1, 0) succeeds and no start time is
        // readable, so without the range guard the probe returns true and the
        // caller broadcasts SIGTERM.
        assert!(!pid_is_ours(u32::MAX, None), "u32::MAX must never be ours");
        assert!(
            !pid_is_ours(i32::MAX as u32 + 1, Some(123)),
            "anything past i32::MAX wraps negative"
        );
        assert!(
            pid_confirmed_dead(u32::MAX),
            "out-of-range is never running"
        );
    }

    #[test]
    fn recycle_and_death_each_demand_positive_evidence() {
        // The distinction `pid_gone_within` rests on. `!pid_is_ours` is NOT a
        // recycle test: it is also false for a live-but-unsignalable process, and
        // treating that as "gone" reports a clean stop over a running worker.
        let me = std::process::id();
        let Some(st) = process_start_time(me) else {
            return; // platform without start-time support
        };

        // Alive and ours: neither dead nor recycled.
        assert!(!pid_confirmed_dead(me), "a live pid is not dead");
        assert!(
            !pid_recycled(me, Some(st)),
            "matching token is not a recycle"
        );

        // Alive with a mismatched token: a positive recycle finding.
        assert!(
            pid_recycled(me, Some(st.wrapping_add(1))),
            "reachable + differing token is a recycle"
        );

        // No recorded token: no basis to claim a recycle either way.
        assert!(!pid_recycled(me, None), "no token -> no recycle verdict");

        // A dead pid is dead, and is never *also* reported as recycled -- the
        // caller must not be able to reach "gone" through an unproven path.
        let dead = 0x7fff_fff0u32;
        assert!(pid_confirmed_dead(dead), "unused high pid reads as dead");
        assert!(
            !pid_recycled(dead, Some(st)),
            "dead is not a recycle finding"
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
                legacy_provider: "codex".into(),
                provider: None,
                model: None,
                effort: None,
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
                crown_level: None,
                crown_scope: None,
                crown_grantor: None,
                route_settings_path: None,
                fno_id: None,
                delivery_policy: None,
            });
        })
        .unwrap();
        let mut st = AgentState::new_pty("recycled");
        st.status = AgentStatus::Live;
        state::write_state_atomic(&home.state_json("recycled"), &st).unwrap();

        let report = recover(&home, &emitter).expect("startup recovery");
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

        let report = recover(&home, &emitter).expect("startup recovery");
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
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            effort: None,
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
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
        });
        assert_eq!(derive_short_id("worker-A", &reg), "workerA1");
    }

    // --- plan_reconcile (US6.9): tri-state, status-aware transitions, budget ---

    fn rentry(name: &str, status: AgentStatus, last_reconciled: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: name.into(),
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            effort: None,
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
            crown_level: None,
            crown_scope: None,
            crown_grantor: None,
            route_settings_path: None,
            fno_id: None,
            delivery_policy: None,
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
        let (changes, out) = plan_reconcile(&entries, |_| Ok(false), || false, |_| true, |_| false);
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
        let (changes, out) = plan_reconcile(&entries, |_| Ok(true), || false, |_| true, |_| false);
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
        let (changes, out) = plan_reconcile(&entries, |_| Ok(false), || false, |_| true, |_| false);
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
        let (changes, out) = plan_reconcile(&entries, |_| Ok(true), || false, |_| true, |_| false);
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
        let (changes, out) = plan_reconcile(&entries, |_| Ok(true), || false, |_| true, |_| true);
        assert_eq!(
            changes[0].new_status, None,
            "a bg thread claude's daemon still lists must not be reaped to exited"
        );
        assert!(out.updated.is_empty());

        // Absent from the roster == genuinely gone: the ask reap still applies,
        // so this is a liveness check, not a blanket exemption for claude rows.
        let (changes, out) = plan_reconcile(&entries, |_| Ok(true), || false, |_| true, |_| false);
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

        late_bind_codex_sessions(&home, &emitter, &|_| {
            panic!("an already-bound row must not be re-probed")
        })
        .unwrap();

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

    /// A stub family-1 probe answer for the tests that only pin the state.
    /// `observed_model` null here stands for "this probe did not answer it",
    /// which the row renders as `no-transcript` rather than inventing a model.
    ///
    /// `reachability: None` deliberately exercises the COMPATIBILITY FALLBACK in
    /// `rendered_status_from_truth` (a `fno` too old to emit the verdict), which
    /// is what keeps these pre-existing state-mapping assertions meaningful.
    /// `probe_reachable` below covers the current wire.
    fn probe(state: &str) -> Option<crate::claude_ask::TruthProbe> {
        Some(crate::claude_ask::TruthProbe {
            state: state.into(),
            reachability: None,
            basis: None,
            last_activity_age_s: None,
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        })
    }

    /// A probe carrying the shared verdict, as a current `fno` emits it.
    fn probe_with_verdict(
        state: &str,
        reachability: &str,
    ) -> Option<crate::claude_ask::TruthProbe> {
        Some(crate::claude_ask::TruthProbe {
            state: state.into(),
            reachability: Some(reachability.into()),
            basis: Some("transcript".into()),
            last_activity_age_s: Some(12.0),
            last_event_at: Some("2026-08-15T17:00:00+00:00".into()),
            last_message: Some(
                "Still growing (101 lines, 26 percent through the pytest run)".into(),
            ),
            observed_model: Value::Null,
        })
    }

    fn probe_with_age(
        state: &str,
        reachability: &str,
        age_s: Option<f64>,
    ) -> Option<crate::claude_ask::TruthProbe> {
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
            rendered_status_from_truth(
                probe_with_verdict("working", "unreachable").as_ref(),
                false
            ),
            "orphaned",
            "a falsified row must not render live merely because its transcript is recent"
        );
        // And silence is not death: the verdict says unknown where the legacy
        // state mapping said orphaned.
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("stalled", "unknown").as_ref(), false),
            "unknown"
        );
        assert_eq!(
            rendered_status_from_truth(probe("stalled").as_ref(), false),
            "orphaned",
            "the fallback keeps its old meaning for a fno too old to send a verdict"
        );
    }

    #[test]
    fn a_confirmed_live_pid_overrides_an_unresolvable_truth_probe_but_never_a_falsifier() {
        // x-9de7 task 3 (the second render path the king's live measurement
        // found): a row with no short_id/harness_session_id resolves through
        // its bare name, which the truth probe can never find -- "unknown" is
        // then a statement about missing session identity, not about whether
        // the worker is alive. A confirmed-live pid is its own measurement.
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("working", "unknown").as_ref(), true),
            "live",
            "an unresolvable probe + a confirmed-live pid must render live, not unknown"
        );
        assert_eq!(
            rendered_status_from_truth(None, true),
            "live",
            "no probe at all (too-old fno / shellout failure) + a live pid still renders live"
        );
        // The override is scoped to the two unmeasured shapes ONLY. A probe
        // that positively falsified the row (unreachable) is never raised by
        // a live pid -- that would contradict the monotone-lowering rule
        // task 4 exists to enforce.
        assert_eq!(
            rendered_status_from_truth(probe_with_verdict("working", "unreachable").as_ref(), true),
            "orphaned",
            "a positive falsifier must never be overridden by pid liveness"
        );
    }

    /// A verdict-carrying probe with an explicit `observed_model`, for the
    /// progress-axis tests below (`probe_with_verdict` above always carries
    /// `Value::Null`, which is `no-transcript` and can never refuse).
    fn probe_observed(
        state: &str,
        reachability: &str,
        observed_model: Value,
    ) -> Option<crate::claude_ask::TruthProbe> {
        Some(crate::claude_ask::TruthProbe {
            state: state.into(),
            reachability: Some(reachability.into()),
            basis: Some("transcript".into()),
            last_activity_age_s: Some(12.0),
            last_event_at: None,
            last_message: None,
            observed_model,
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
    fn progress_deliberately_wedged_open_turn_is_live_but_not_advancing() {
        let probe = probe_with_age("working", "reachable", Some(STALE_ATTENTION_S + 1.0));
        assert_eq!(
            rendered_status_from_truth(probe.as_ref(), true),
            "live",
            "the process and reachability axes still say live"
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
                dead_row_grace_cwd: PathBuf::from("/dev/null"),
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
                dead_row_grace_cwd: PathBuf::from("/dev/null"),
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
        let p = PathBuf::from(format!("/tmp/fnosb{tag}{}_{n}", std::process::id()));
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
                legacy_provider: "claude".into(),
                provider: None,
                model: None,
                effort: None,
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
                crown_level: None,
                crown_scope: None,
                crown_grantor: None,
                route_settings_path: None,
                fno_id: None,
                delivery_policy: None,
            });
        })
        .unwrap();
    }

    /// A row with no `short_id` and no `harness_session_id` -- the shape
    /// `registry_truth_handle` cannot resolve to anything a truth probe can
    /// find, matching a pane-hosted codex row that never bound a session id.
    fn seed_bare_row(name: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            legacy_provider: String::new(),
            provider: None,
            model: None,
            effort: None,
            harness: Some("codex".into()),
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
            created_at: "2026-06-09T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
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
    fn list_row_key_set_matches_shared_contract() {
        const CONTRACT: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/agents-list-row.json"
        ));
        let contract: Value = serde_json::from_str(CONTRACT).expect("contract is valid JSON");
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
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({}));

        let response = handle_list_with_truth(&ctx, &req, |_handle| probe("working"));
        let result = response.result().unwrap();
        let row = &result["agents"][0];

        let actual: std::collections::BTreeSet<String> =
            row.as_object().unwrap().keys().cloned().collect();
        assert_eq!(actual, expected, "list row key set drifted from contract");

        // Presence in the key set is not the bug being guarded: a key that is
        // always null is the same lie in a different shape. Assert the values
        // reach the row.
        assert_eq!(row["harness"], "claude");
        // No key whose name says vendor and whose value is a harness (AC8).
        // Dropping it from one emitter only would be worse than keeping it
        // everywhere: the field would then be present or absent depending on
        // which reader answered.
        assert!(row.get("provider").is_none());
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| {
            probe_with_verdict("working", "reachable")
        });
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
        let response = handle_list_with_truth(&ctx, &req, |_handle| None);
        let row = &response.result().unwrap()["agents"][0];
        assert!(row["reachability"].is_null());
        assert!(row["basis"].is_null());
        assert!(row["last_activity_age_s"].is_null());
        assert!(row["last_event_at"].is_null());
        assert!(row["last_message"].is_null());

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_progress_filter_is_independent_from_status() {
        let home = short_home("listprogressfilter");
        seed_stream_row(&home, "worker-progress", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));

        let parked = Request::new(1, "agent.list", json!({"progress": "parked"}));
        let response = handle_list_with_truth(&ctx, &parked, |_handle| {
            probe_with_verdict("done", "reachable")
        });
        assert_eq!(
            response.result().unwrap()["agents"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let advancing = Request::new(2, "agent.list", json!({"progress": "advancing"}));
        let response = handle_list_with_truth(&ctx, &advancing, |_handle| {
            probe_with_verdict("done", "reachable")
        });
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| {
            Some(crate::claude_ask::TruthProbe {
                state: "working".into(),
                reachability: Some("reachable".into()),
                basis: Some("transcript".into()),
                last_activity_age_s: Some(3.5),
                last_event_at: None,
                last_message: None,
                observed_model: json!({
                    "kind": "observed", "model": "glm-5.2", "samples": 300
                }),
            })
        });
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["observed_model"]["model"], "glm-5.2");
        assert_eq!(row["observed_model"]["kind"], "observed");

        // A probe that did not answer must not leave a bare null: an absent
        // value is what an operator correctly reads as proving nothing, which
        // is the exact misreading this field exists to end.
        let response = handle_list_with_truth(&ctx, &req, |_handle| probe("working"));
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["observed_model"], json!({"kind": "no-transcript"}));

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The end-to-end shape of the king's live measurement (x-9de7 task 3): a
    /// codex pane row with no short_id and no harness_session_id -- the exact
    /// specimen -- resolves through `registry_truth_handle` to its bare name,
    /// which no truth probe can ever find. `status` must still read `live`
    /// when the pid demonstrably is, not `unknown`.
    #[test]
    fn list_status_is_live_for_an_unresolvable_row_with_a_confirmed_live_pid() {
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| None);
        let row = &response.result().unwrap()["agents"][0];
        assert_eq!(row["status"], "live");

        std::fs::remove_dir_all(home.root()).ok();
    }

    /// The pid-liveness override never fires FOR a row the probe positively
    /// falsified, and never fires when the pid is confirmed dead -- only
    /// "the probe could not measure" plus "the pid is confirmed live" together
    /// produce the override.
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| None);
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| probe("working"));
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| probe("working"));
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

        let response = handle_list_with_truth(&ctx, &req, |_handle| probe("working"));
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
        let req = Request::new(1, "agent.list", json!({"status": "live"}));

        let response = handle_list_with_truth(&ctx, &req, |_handle| probe("working"));
        let result = response.result().unwrap();
        let agents = result["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["status"], "live");

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn list_queries_family1_by_session_identity_not_custom_name() {
        let home = short_home("listidentity");
        seed_stream_row(&home, "custom-worker-name", "abc12345");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"status": "live"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(&ctx, &req, |handle| {
            seen.borrow_mut().push(handle.to_string());
            probe("working")
        });

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
        let req = Request::new(1, "agent.list", json!({"status": "live"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(&ctx, &req, |handle| {
            seen.borrow_mut().push(handle.to_string());
            probe("working")
        });

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
        let req = Request::new(1, "agent.list", json!({"status": "live"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(&ctx, &req, |handle| {
            seen.borrow_mut().push(handle.to_string());
            probe("working")
        });

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
        })
        .unwrap();
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(1, "agent.list", json!({"provider": "codex"}));
        let seen = std::cell::RefCell::new(Vec::new());

        let response = handle_list_with_truth(&ctx, &req, |handle| {
            seen.borrow_mut().push(handle.to_string());
            probe("working")
        });

        assert!(response.result().is_some());
        assert_eq!(
            seen.into_inner(),
            vec!["bbbbbbbb-1111-2222-3333-444444444444"]
        );
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
