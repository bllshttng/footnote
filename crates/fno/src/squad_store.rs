//! Persisted named squads (`~/.fno/squads.json`): the durable half of the
//! session-scoped [`crate::squad::Squad`] model.
//!
//! Only explicit NAMED workspaces persist (`NewSquad` / bulk recruit). An
//! attach-born origin squad re-derives from a fresh attach and a member's PANE
//! is ephemeral (re-created at restore, never stored); the store holds only the
//! workspace name, its origins, and its member attach-ids. Identity is `name`,
//! unique in the file (Locked Decision 4).
//!
//! Same rules as the rest of this crate: the FILE is the contract (no
//! cross-crate import), all I/O degrades the persistence, never the session -
//! a corrupt file quarantines, a contended lock skips the write, a disk-full
//! write returns an error the caller notices once. Production resolves the path
//! via `FNO_AGENTS_HOME` (mirroring [`crate::agents_view::registry_path`]);
//! tests redirect it with a per-thread override so they never touch a real home
//! and never mutate the shared environment.

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// A per-thread store-path override for tests, so a store-touching test never
// mutates the process-global environment (a `set_var` there would race any
// concurrent `getenv` in a sibling test - a real data race). Cargo runs each
// test on its own thread, so a thread-local gives every test full isolation
// with no lock. Set via `set_test_path`; cleared on scope exit.
#[cfg(test)]
thread_local! {
    static TEST_PATH: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point this thread's store at `dir/squads.json` (test-only).
#[cfg(test)]
pub(crate) fn set_test_path(dir: &std::path::Path) {
    TEST_PATH.with(|c| *c.borrow_mut() = Some(dir.join("squads.json")));
}

/// Clear this thread's store override (test-only).
#[cfg(test)]
pub(crate) fn clear_test_path() {
    TEST_PATH.with(|c| *c.borrow_mut() = None);
}

/// The only store schema this build understands. An unknown version is treated
/// exactly like a corrupt file (quarantine + fresh) rather than guessed at
/// (Discretion 5).
pub const STORE_VERSION: u32 = 1;

/// Non-blocking flock retry budget: try, then a handful of short sleeps, then
/// give up and skip the write (never a blocking wait on the caller). Same
/// posture as `squad.rs`'s `GIT_TIMEOUT` - a contended NFS home degrades
/// persistence, it never freezes the core loop.
const FLOCK_RETRIES: u32 = 5;
const FLOCK_SLEEP: Duration = Duration::from_millis(20);

/// One persisted member: the `claude attach <id>` jobId plus whether the
/// worker has died (a tombstone survives restarts as a dimmed row until the
/// operator dismisses it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMember {
    pub attach_id: String,
    #[serde(default)]
    pub tombstone: bool,
    /// (x-0f9d US4) The name of the tab hosting this member's pane at store
    /// time, so a chosen tab name survives a mux restart: restore names the
    /// re-derived tab from it. Re-derived fresh on every persist so a rename is
    /// captured. `#[serde(default)]` keeps a pre-x-0f9d store readable (absent
    /// -> `None` -> the tab restores unnamed, exactly as before) and holds
    /// STORE_VERSION at 1 (an additive field never quarantines existing squads).
    #[serde(default)]
    pub tab_name: Option<String>,
}

/// One persisted named workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSquad {
    pub name: String,
    #[serde(default)]
    pub origins: Vec<String>,
    #[serde(default)]
    pub members: Vec<StoredMember>,
    /// A cosmetic `YYYY-MM-DDThh:mm:ssZ` stamp, preserved across upserts.
    #[serde(default)]
    pub created_at: String,
    /// (x-c4d4) The layout spec of each template-managed, named tab in this
    /// squad. Restore re-applies these to rebuild the template topology (US8),
    /// instead of the one-tab-per-member fallback. `#[serde(default)]` keeps a
    /// pre-x-c4d4 store readable without a `STORE_VERSION` bump (an absent field
    /// loads to `[]`, so no squad is quarantined).
    #[serde(default)]
    pub tab_specs: Vec<StoredTabSpec>,
}

/// One template-managed tab's persisted layout (x-c4d4). Keyed by `tab_name`,
/// the durable tab identity (x-0f9d) - an unnamed tab has no stable key and is
/// never persisted. `spec` is the SAME struct `LayoutApply` consumes, so restore
/// is a plain re-apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTabSpec {
    pub tab_name: String,
    pub spec: crate::proto::LayoutSpec,
}

/// The lifecycle state of a tracked EXTERNAL (claude-daemon) row (x-7561). A
/// LIVE external row is never persisted (the daemon roster owns it); a record
/// is born when we act on one. `stopping`/`removing` are in-flight (a spawn is
/// or was outstanding); `stopped` is the terminal tombstone `x` can rm;
/// `failed`/`unknown` are safe retryable rest states. Declaration order is
/// irrelevant (serde uses the name), but kept in lifecycle order for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalState {
    Stopping,
    Stopped,
    Removing,
    Failed,
    Unknown,
}

/// One tracked external-row lifecycle record. Identity is `attach_id` (8-hex);
/// `name`/`cwd` are cosmetic display/routing snapshots, never authority
/// (Locked Decision 6). `generation` bumps on every begin-stop/begin-rm so a
/// stale subprocess completion can never overwrite a newer action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalLifecycle {
    pub attach_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cwd: String,
    pub state: ExternalState,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub updated_at: String,
    /// A bounded failure reason for `failed` / a retry hint; `None` otherwise.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    squads: Vec<StoredSquad>,
    /// (x-7561) Machine-global external-row lifecycle tombstones. A defaulted
    /// field on the version-1 object: a v1 reader without it stays wire-tolerant
    /// and STORE_VERSION does not bump (which would quarantine existing squads).
    #[serde(default)]
    external_lifecycle: Vec<ExternalLifecycle>,
}

/// The outcome of a durable compare-and-set gate (x-7561). `Committed` carries
/// the new action generation the caller correlates the subprocess result
/// against; `Refused` is a fail-closed reason (no spawn) that is NOT a
/// persistence error. An `io::Err` from the CAS helper is a persistence failure
/// (AC2-FR): no spawn, the row keeps its prior state, notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleCas {
    Committed { generation: u64 },
    Refused(String),
}

/// What [`load`] read: the (member-validated) squads plus an optional one-line
/// notice for the operator (a quarantine, or dropped hostile ids).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub squads: Vec<StoredSquad>,
    /// (x-7561) The tracked external-row lifecycle tombstones, `attach_id`
    /// validated exactly like squad members (a malformed id never reaches an
    /// argv). Empty when the store has none.
    pub external_lifecycle: Vec<ExternalLifecycle>,
    pub notice: Option<String>,
}

/// The store file: a sibling of the registry under `FNO_AGENTS_HOME`, else
/// `$HOME/.fno/squads.json`. Machine-global because a squad spans repos.
pub fn squads_path() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = TEST_PATH.with(|c| c.borrow().clone()) {
        return p;
    }
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("squads.json");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("squads.json")
}

/// A jobId is exactly 8 ascii-hex digits (the `claude attach` gate). File
/// content is untrusted input, so a malformed id must never reach an argv
/// (epic Boundaries; AC2-ERR): it is dropped at load, before restore can spawn.
pub fn valid_attach_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Load the store for restore. A missing/empty file is a fresh store (no
/// notice). A corrupt file or unknown version is renamed aside
/// (`squads.json.corrupt-<secs>`) and read as empty (AC1-ERR: never refuse to
/// start). Members with a malformed `attach_id` are dropped with a notice
/// (AC2-ERR).
pub fn load() -> Loaded {
    let path = squads_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Loaded::default(), // missing / unreadable: fresh
    };
    if raw.trim().is_empty() {
        return Loaded::default();
    }
    let parsed: Option<StoreFile> = serde_json::from_str(&raw).ok();
    let file = match parsed {
        Some(f) if f.version == STORE_VERSION => f,
        _ => {
            // Corrupt JSON or a version this build does not understand: move it
            // aside so the next write starts clean, and tell the operator.
            let stamp = now_secs();
            let aside = path.with_file_name(format!("squads.json.corrupt-{stamp}"));
            let _ = std::fs::rename(&path, &aside);
            return Loaded {
                notice: Some(format!(
                    "quarantined corrupt squads.json to {}",
                    aside.display()
                )),
                ..Loaded::default()
            };
        }
    };
    let mut dropped = 0usize;
    let squads = file
        .squads
        .into_iter()
        .map(|mut sq| {
            let before = sq.members.len();
            sq.members.retain(|m| valid_attach_id(&m.attach_id));
            dropped += before - sq.members.len();
            sq
        })
        .collect();
    // Same argv-safety gate for lifecycle records: a malformed attach_id never
    // survives load, so a reconcile / rm can never shell it (epic Boundaries).
    let before_lc = file.external_lifecycle.len();
    let external_lifecycle: Vec<ExternalLifecycle> = file
        .external_lifecycle
        .into_iter()
        .filter(|r| valid_attach_id(&r.attach_id))
        .collect();
    let dropped_lc = before_lc - external_lifecycle.len();
    let notice = match (dropped, dropped_lc) {
        (0, 0) => None,
        (s, 0) => Some(format!("dropped {s} malformed squad member(s)")),
        (0, l) => Some(format!("dropped {l} malformed lifecycle record(s)")),
        (s, l) => Some(format!(
            "dropped {s} malformed squad member(s) and {l} lifecycle record(s)"
        )),
    };
    Loaded {
        squads,
        external_lifecycle,
        notice,
    }
}

/// Insert-or-replace the entry named `name`, preserving its `created_at` if it
/// already exists (else stamping now). Write-through for `NewSquad`, recruit,
/// member close, and tombstone.
pub fn upsert(name: &str, origins: &[String], members: &[StoredMember]) -> io::Result<()> {
    mutate(|squads| {
        let existing = squads.iter().find(|s| s.name == name);
        let created_at = existing
            .map(|s| s.created_at.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(now_iso);
        // Preserve template tab specs (owned by set_tab_specs, not this path)
        // across a membership upsert - the struct is rebuilt fresh, so an
        // un-carried field would be silently wiped (x-c4d4).
        let tab_specs = existing.map(|s| s.tab_specs.clone()).unwrap_or_default();
        squads.retain(|s| s.name != name);
        squads.push(StoredSquad {
            name: name.to_string(),
            origins: origins.to_vec(),
            members: members.to_vec(),
            created_at,
            tab_specs,
        });
    })
}

/// Set the template tab specs for `name` (x-c4d4), preserving its other fields.
/// Inserts a minimal entry if the squad is not yet persisted (a template applied
/// before any membership write). A store-write failure is the caller's to treat
/// as degraded persistence (the live layout stands).
pub fn set_tab_specs(name: &str, tab_specs: &[StoredTabSpec]) -> io::Result<()> {
    mutate(|squads| {
        if let Some(s) = squads.iter_mut().find(|s| s.name == name) {
            s.tab_specs = tab_specs.to_vec();
        } else {
            squads.push(StoredSquad {
                name: name.to_string(),
                origins: Vec::new(),
                members: Vec::new(),
                created_at: now_iso(),
                tab_specs: tab_specs.to_vec(),
            });
        }
    })
}

/// Delete the entry named `name` (a user-closed / removed workspace). A name
/// not present is a silent no-op.
pub fn remove(name: &str) -> io::Result<()> {
    mutate(|squads| squads.retain(|s| s.name != name))
}

/// Rename `old` -> `new` in one locked mutation, carrying `created_at` across
/// (a `RenameSquad`). Any pre-existing `new` entry is overwritten.
pub fn rename(
    old: &str,
    new: &str,
    origins: &[String],
    members: &[StoredMember],
) -> io::Result<()> {
    mutate(|squads| {
        let existing = squads.iter().find(|s| s.name == old || s.name == new);
        let created_at = existing
            .map(|s| s.created_at.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(now_iso);
        let tab_specs = existing.map(|s| s.tab_specs.clone()).unwrap_or_default();
        squads.retain(|s| s.name != old && s.name != new);
        squads.push(StoredSquad {
            name: new.to_string(),
            origins: origins.to_vec(),
            members: members.to_vec(),
            created_at,
            tab_specs,
        });
    })
}

/// Begin an external STOP (x-7561, AC2-FR gate): under the store lock, move the
/// record for `id` to `stopping` with a FRESH generation, snapshotting
/// `name`/`cwd` (cosmetic). A LIVE row carries no record yet, so an absent id
/// inserts one at generation 1. Refused (no state change) when the current state
/// cannot be stopped - `stopped` (use rm) or `removing` (rm in flight);
/// `failed`/`unknown`/`stopping` all permit a stop retry. Returns the committed
/// generation the caller correlates the subprocess result against. An `io::Err`
/// is a persistence failure - the caller must NOT spawn (AC2-FR).
pub fn begin_external_stop(id: &str, name: &str, cwd: &str) -> io::Result<LifecycleCas> {
    let mut outcome = LifecycleCas::Refused("internal".into());
    mutate_lifecycle(|records| {
        outcome = match records.iter_mut().find(|r| r.attach_id == id) {
            None => {
                records.push(ExternalLifecycle {
                    attach_id: id.to_string(),
                    name: name.to_string(),
                    cwd: cwd.to_string(),
                    state: ExternalState::Stopping,
                    generation: 1,
                    updated_at: now_iso(),
                    reason: None,
                });
                LifecycleCas::Committed { generation: 1 }
            }
            Some(r) => match r.state {
                ExternalState::Stopped => {
                    LifecycleCas::Refused(format!("{name} already stopped - remove it instead"))
                }
                ExternalState::Removing => {
                    LifecycleCas::Refused(format!("{name} is being removed"))
                }
                // An in-flight stop must NOT launch a second `claude stop` (codex
                // P1): a duplicate spawn discards the first completion and assumes
                // stop is concurrency-safe. Only a SETTLED rest state (failed /
                // unknown) is retryable; a stuck `stopping` is made retryable by
                // startup reconciliation flipping it to failed, not by a re-press.
                ExternalState::Stopping => {
                    LifecycleCas::Refused(format!("{name} is already stopping"))
                }
                ExternalState::Failed | ExternalState::Unknown => {
                    r.generation += 1;
                    r.state = ExternalState::Stopping;
                    r.name = name.to_string();
                    r.cwd = cwd.to_string();
                    r.reason = None;
                    r.updated_at = now_iso();
                    LifecycleCas::Committed {
                        generation: r.generation,
                    }
                }
            },
        };
    })?;
    Ok(outcome)
}

/// Begin an external RM (x-7561, stop-then-rm ordering): refuse unless the
/// record is `stopped`. On commit, bump generation and set `removing` before the
/// caller spawns `claude rm`. A live/`stopping`/`failed` target refuses with
/// `stop it first`; `unknown` refuses with `state unknown; retry stop`; an
/// absent record refuses (nothing to remove). Same persistence-error contract as
/// [`begin_external_stop`].
pub fn begin_external_rm(id: &str) -> io::Result<LifecycleCas> {
    let mut outcome = LifecycleCas::Refused(format!("no such stopped row: {id}"));
    mutate_lifecycle(|records| {
        if let Some(r) = records.iter_mut().find(|r| r.attach_id == id) {
            outcome = match r.state {
                ExternalState::Stopped => {
                    r.generation += 1;
                    r.state = ExternalState::Removing;
                    r.reason = None;
                    r.updated_at = now_iso();
                    LifecycleCas::Committed {
                        generation: r.generation,
                    }
                }
                ExternalState::Unknown => LifecycleCas::Refused("state unknown; retry stop".into()),
                _ => LifecycleCas::Refused("stop it first".into()),
            };
        }
    })?;
    Ok(outcome)
}

/// Record a subprocess completion (x-7561). Applied ONLY when the record exists,
/// its `generation` matches, AND its current state is the in-flight `action` -
/// so a stale retry's late completion (older generation, or a state a newer
/// action already moved on from) is ignored and can never overwrite a newer
/// action. `stopping`: ok -> `stopped`, err -> `failed`. `removing`: ok ->
/// deleted, err -> `stopped` (rm stays retryable). `reason` is a bounded blurb
/// on the err paths.
pub fn complete_external(
    id: &str,
    generation: u64,
    action: ExternalState,
    ok: bool,
    reason: Option<String>,
) -> io::Result<()> {
    mutate_lifecycle(|records| {
        let Some(idx) = records
            .iter()
            .position(|r| r.attach_id == id && r.generation == generation && r.state == action)
        else {
            return; // stale generation / state moved on / gone: ignore
        };
        match (action, ok) {
            (ExternalState::Removing, true) => {
                records.remove(idx);
                return;
            }
            (ExternalState::Stopping, true) => records[idx].state = ExternalState::Stopped,
            (ExternalState::Stopping, false) => records[idx].state = ExternalState::Failed,
            (ExternalState::Removing, false) => records[idx].state = ExternalState::Stopped,
            _ => return, // action is only ever Stopping/Removing
        }
        records[idx].reason = if ok { None } else { reason };
        records[idx].updated_at = now_iso();
    })
}

/// Apply the startup reconcile ATOMICALLY under the store lock (x-7561): the
/// `claude agents` liveness query runs off-lock (it must - a subprocess cannot
/// be awaited while holding the flock), but the load -> compute -> write is
/// serialized here so a concurrent operator action is never clobbered
/// (lost-update). `baseline` is the `attach_id -> generation` snapshot taken
/// BEFORE the query; under the lock, a record whose generation still matches its
/// baseline is fed to `reconcile` (the pure `agents_view` table), while a record
/// the baseline never saw or whose generation ADVANCED (a concurrent stop/rm
/// owns it) is left untouched - reconciling it against a pre-action liveness
/// snapshot would drop the action's completion. Returns the reconcile notices.
pub fn reconcile_lifecycle<F>(
    baseline: &std::collections::HashMap<String, u64>,
    reconcile: F,
) -> io::Result<Vec<String>>
where
    F: FnOnce(Vec<ExternalLifecycle>) -> (Vec<ExternalLifecycle>, Vec<String>),
{
    let mut notices = Vec::new();
    mutate_lifecycle(|records| {
        let (reconcilable, mut untouched): (Vec<_>, Vec<_>) = std::mem::take(records)
            .into_iter()
            .partition(|r| baseline.get(&r.attach_id) == Some(&r.generation));
        let (reconciled, ns) = reconcile(reconcilable);
        notices = ns;
        untouched.extend(reconciled);
        *records = untouched;
    })?;
    Ok(notices)
}

/// The locked read-modify-write core: acquire an exclusive, NON-BLOCKING lock
/// on a sibling lockfile (bounded retry, then give up), re-read the current
/// file, apply `f`, and atomically rename a tmp over the target. Two mux
/// servers serialize on the lockfile; last writer wins per squad name. A
/// corrupt file read here is treated as empty (the load path owns quarantine),
/// so a write never fails on unreadable prior content.
fn mutate(f: impl FnOnce(&mut Vec<StoredSquad>)) -> io::Result<()> {
    mutate_file(|sf| f(&mut sf.squads))
}

/// The lifecycle-collection twin of [`mutate`] (x-7561): the SAME locked atomic
/// read-modify-write, applying `f` to `external_lifecycle` while preserving
/// `squads` byte-for-byte. Both collections ride one version-1 object, so a
/// squad write can never drop a lifecycle record and vice-versa.
fn mutate_lifecycle(f: impl FnOnce(&mut Vec<ExternalLifecycle>)) -> io::Result<()> {
    mutate_file(|sf| f(&mut sf.external_lifecycle))
}

/// The locked read-modify-write core over the WHOLE [`StoreFile`]: acquire the
/// exclusive non-blocking lock, re-read the current file (empty/absent = fresh,
/// any other read/parse error FAILS LOUD rather than clobber unread content),
/// apply `f` to both collections at once, pin the version, and atomically
/// rename a tmp over the target. `mutate` / `mutate_lifecycle` are thin views
/// onto it, so every mutation preserves both collections.
fn mutate_file(f: impl FnOnce(&mut StoreFile)) -> io::Result<()> {
    let path = squads_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock_path = path.with_file_name("squads.json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let _guard = FlockGuard::acquire(lock)?;

    let mut file = match std::fs::read_to_string(&path) {
        Ok(raw) if raw.trim().is_empty() => StoreFile::default(),
        Ok(raw) => serde_json::from_str::<StoreFile>(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => StoreFile::default(),
        Err(e) => return Err(e),
    };
    f(&mut file);
    file.version = STORE_VERSION;

    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_file_name(format!("squads.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    // Atomic rename: a concurrent reader sees either the old or the new file,
    // never a torn one (AC1-FR).
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Holds an advisory `flock` for the life of the guard, releasing on drop.
/// `pub(crate)` so the sibling view store serializes its own read-modify-write
/// on the same proven primitive instead of hand-rolling a second one.
pub(crate) struct FlockGuard(std::fs::File);

impl FlockGuard {
    pub(crate) fn acquire(file: std::fs::File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        for _ in 0..FLOCK_RETRIES {
            // SAFETY: fd is owned by `file`, valid for this call.
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(FlockGuard(file));
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err);
            }
            std::thread::sleep(FLOCK_SLEEP);
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "squads.json lock contended",
        ))
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        // SAFETY: fd is owned by self.0, valid until the drop completes.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current time as a `YYYY-MM-DDThh:mm:ssZ` UTC stamp (the inverse of
/// `agents_view::rfc3339_like_to_secs`; Hinnant civil-from-days). Cosmetic - a
/// clock before the epoch just stamps the epoch.
fn now_iso() -> String {
    epoch_to_iso(now_secs())
}

fn epoch_to_iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, se) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Hinnant civil_from_days: days since 1970-01-01 -> (y, m, d).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch store dir installed via the per-thread path override, so the
    /// store never touches a real file AND never mutates the process
    /// environment (no cross-test env race). Cleared on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fno-squadstore-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            super::set_test_path(&dir);
            Scratch(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("squads.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            super::clear_test_path();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn m(id: &str) -> StoredMember {
        StoredMember {
            attach_id: id.into(),
            tombstone: false,
            tab_name: None,
        }
    }

    #[test]
    fn missing_file_is_a_fresh_store() {
        let _s = Scratch::new("missing");
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.is_none(), "a missing file is silent");
    }

    #[test]
    fn pre_xc4d4_store_loads_without_tab_specs_field() {
        // AC9: a store written before x-c4d4 has no `tab_specs` key. It must load
        // unquarantined (STORE_VERSION unchanged), defaulting tab_specs to empty.
        let s = Scratch::new("no-tab-specs");
        // Hand-write a v1 squad object WITHOUT the tab_specs key.
        let raw = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[],"created_at":"2026-07-11T00:00:00Z"}]}"#;
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert!(loaded.squads[0].tab_specs.is_empty(), "tab_specs defaults to empty");
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn set_tab_specs_persists_and_upsert_preserves_it() {
        use crate::proto::{LayoutSpec, SlotBinding, TemplateName};
        let _s = Scratch::new("tab-specs");
        let spec = StoredTabSpec {
            tab_name: "grid".into(),
            spec: LayoutSpec {
                template: TemplateName::MainLeft,
                slots: vec![SlotBinding::Fno("S1".into()), SlotBinding::Shell],
            },
        };
        upsert("w", &["/r".into()], &[m("c19cd2c3")]).unwrap();
        set_tab_specs("w", std::slice::from_ref(&spec)).unwrap();
        assert_eq!(load().squads[0].tab_specs, vec![spec.clone()]);
        // A later membership upsert must NOT wipe the template specs (they are
        // owned by set_tab_specs, and upsert rebuilds the struct fresh).
        upsert("w", &["/r".into()], &[m("c19cd2c3"), m("deadbeef")]).unwrap();
        let after = load();
        assert_eq!(after.squads[0].members.len(), 2, "membership updated");
        assert_eq!(after.squads[0].tab_specs, vec![spec], "tab_specs preserved across upsert");
    }

    #[test]
    fn upsert_then_load_roundtrips_and_preserves_created_at() {
        let _s = Scratch::new("roundtrip");
        upsert("harden", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        let first = load();
        assert_eq!(first.squads.len(), 1);
        let created = first.squads[0].created_at.clone();
        assert!(created.ends_with('Z') && created.len() == 20, "{created}");
        // A second upsert (new members) keeps the original created_at.
        upsert("harden", &["/repo".into()], &[m("c19cd2c3"), m("deadbeef")]).unwrap();
        let second = load();
        assert_eq!(second.squads.len(), 1, "upsert replaces, never dupes");
        assert_eq!(second.squads[0].members.len(), 2);
        assert_eq!(second.squads[0].created_at, created, "created_at preserved");
    }

    #[test]
    fn tab_name_roundtrips_and_absent_field_loads_none() {
        // x-0f9d US4: a member's tab_name persists and reloads; a pre-x-0f9d
        // store written without the field is wire-tolerant (loads as None ->
        // the tab restores unnamed), so STORE_VERSION stays 1.
        let s = Scratch::new("tabname");
        let mut named = m("c19cd2c3");
        named.tab_name = Some("reviews".into());
        upsert("work", &["/repo".into()], &[named]).unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads[0].members[0].tab_name.as_deref(),
            Some("reviews"),
            "chosen tab name round-trips"
        );

        // A hand-written v1 store with no tab_name field must not quarantine.
        std::fs::write(
            s.file(),
            r#"{"version":1,"squads":[{"name":"legacy","origins":[],"members":[{"attach_id":"deadbeef","tombstone":false}],"created_at":""}]}"#,
        )
        .unwrap();
        let loaded = load();
        assert!(loaded.notice.is_none(), "absent field is not corruption");
        assert_eq!(
            loaded.squads[0].members[0].tab_name, None,
            "absent tab_name -> None"
        );
    }

    #[test]
    fn corrupt_file_is_quarantined_and_read_empty() {
        // AC1-ERR: invalid JSON is renamed aside, not fatal.
        let s = Scratch::new("corrupt");
        std::fs::write(s.file(), "{not valid json").unwrap();
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.as_deref().unwrap().contains("quarantined"));
        assert!(!s.file().exists(), "the corrupt file was moved aside");
        let asides: Vec<_> = std::fs::read_dir(&s.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("squads.json.corrupt-")
            })
            .collect();
        assert_eq!(asides.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn unknown_version_is_quarantined() {
        // Discretion 5: a version this build does not understand takes the
        // quarantine path, never a best-effort parse.
        let s = Scratch::new("version");
        std::fs::write(s.file(), r#"{"version":999,"squads":[]}"#).unwrap();
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.as_deref().unwrap().contains("quarantined"));
    }

    #[test]
    fn hostile_attach_ids_are_dropped_at_load() {
        // AC2-ERR: a member whose attach_id is not 8-hex never survives load,
        // so restore can never spawn it.
        let s = Scratch::new("hostile");
        let file = StoreFile {
            version: STORE_VERSION,
            squads: vec![StoredSquad {
                name: "w".into(),
                origins: vec![],
                members: vec![
                    m("c19cd2c3"),  // good
                    m("; rm -rf"),  // shell metachar
                    m("deadbeef9"), // 9 chars
                    m("GHIJKLmn"),  // non-hex
                ],
                created_at: "2026-07-11T00:00:00Z".into(),
                tab_specs: vec![],
            }],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads[0].members, vec![m("c19cd2c3")]);
        assert!(loaded.notice.as_deref().unwrap().contains("dropped 3"));
    }

    #[test]
    fn remove_and_rename_mutate_by_name() {
        let _s = Scratch::new("remove-rename");
        upsert("a", &[], &[m("11111111")]).unwrap();
        upsert("b", &[], &[m("22222222")]).unwrap();
        rename("a", "aa", &["/x".into()], &[m("11111111")]).unwrap();
        remove("b").unwrap();
        let loaded = load();
        let names: Vec<_> = loaded.squads.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aa"], "a renamed, b removed");
        assert_eq!(loaded.squads[0].origins, vec!["/x".to_string()]);
    }

    #[test]
    fn write_onto_a_corrupt_file_fails_loud_and_never_clobbers() {
        // gemini review: the write path must NOT clobber unreadable content. A
        // corrupt existing file makes upsert fail (Err) rather than overwrite it
        // with just this delta - the load path owns quarantine, not the writer.
        let s = Scratch::new("write-corrupt");
        std::fs::write(s.file(), "{garbage not json").unwrap();
        let before = std::fs::read_to_string(s.file()).unwrap();
        let res = upsert("w", &[], &[m("c19cd2c3")]);
        assert!(res.is_err(), "a write onto corrupt content fails loud");
        assert_eq!(
            std::fs::read_to_string(s.file()).unwrap(),
            before,
            "the corrupt file is left intact, not clobbered"
        );
    }

    #[test]
    fn epoch_to_iso_matches_known_stamps() {
        assert_eq!(epoch_to_iso(0), "1970-01-01T00:00:00Z");
        // 2026-07-11T13:00:00Z -> verified against `date -u -j`.
        assert_eq!(epoch_to_iso(1_783_774_800), "2026-07-11T13:00:00Z");
    }

    #[test]
    fn valid_attach_id_gate() {
        assert!(valid_attach_id("c19cd2c3"));
        assert!(!valid_attach_id("c19cd2c")); // 7
        assert!(!valid_attach_id("c19cd2c33")); // 9
        assert!(!valid_attach_id("c19cd2cg")); // non-hex
        assert!(!valid_attach_id(""));
    }

    fn lc(id: &str) -> Option<ExternalLifecycle> {
        load()
            .external_lifecycle
            .into_iter()
            .find(|r| r.attach_id == id)
    }

    #[test]
    fn squad_and_lifecycle_collections_never_drop_each_other() {
        // The version-1 object carries both collections; a squad write must
        // preserve lifecycle records and a lifecycle CAS must preserve squads.
        let _s = Scratch::new("both-collections");
        upsert("w", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));
        // A SECOND squad write must not clobber the lifecycle record.
        upsert("w2", &[], &[m("11111111")]).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 2, "both squads survive");
        assert_eq!(
            loaded.external_lifecycle.len(),
            1,
            "lifecycle survives a squad write"
        );
        // And a lifecycle CAS must not clobber the squads.
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 2, "squads survive a lifecycle write");
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopped);
    }

    #[test]
    fn begin_stop_inserts_then_bumps_generation_on_retry() {
        // A LIVE row has no record -> insert at generation 1 (stopping). A retry
        // from a rest state bumps the generation, so a stale completion cannot
        // clobber the newer action.
        let _s = Scratch::new("begin-stop-gen");
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));
        // Land it in `failed`, then retry the stop: generation must advance.
        complete_external(
            "deadbeef",
            1,
            ExternalState::Stopping,
            false,
            Some("boom".into()),
        )
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Failed);
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 2 }
        ));
    }

    #[test]
    fn begin_stop_refused_from_stopped_and_removing() {
        // stop-then-rm: a stopped tombstone is removed, not re-stopped; a row
        // already being removed refuses a concurrent stop.
        let _s = Scratch::new("begin-stop-refused");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap();
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap(); // -> stopped
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Refused(_)
        ));
        begin_external_rm("deadbeef").unwrap(); // -> removing (gen 2)
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Refused(_)
        ));
    }

    #[test]
    fn begin_stop_refused_while_already_stopping() {
        // codex P1: a stop in flight must NOT launch a second `claude stop`. A
        // `stopping` record refuses "already stopping" (only failed/unknown rest
        // states retry) - the generation never advances, so the first
        // completion is never orphaned by a duplicate spawn.
        let _s = Scratch::new("begin-stop-inflight");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen1 Stopping
        match begin_external_stop("deadbeef", "ext", "/tmp").unwrap() {
            LifecycleCas::Refused(r) => assert!(r.contains("already stopping")),
            _ => panic!("a second stop while stopping must refuse"),
        }
        assert_eq!(
            lc("deadbeef").unwrap().generation,
            1,
            "generation must not advance"
        );
    }

    #[test]
    fn begin_rm_requires_a_stopped_record() {
        // rm is reachable ONLY from `stopped` (stop-before-rm). A live/stopping
        // row refuses "stop it first"; an unknown row refuses "retry stop"; an
        // absent id refuses.
        let _s = Scratch::new("begin-rm");
        assert!(matches!(
            begin_external_rm("deadbeef").unwrap(),
            LifecycleCas::Refused(_) // absent
        ));
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // -> stopping
        match begin_external_rm("deadbeef").unwrap() {
            LifecycleCas::Refused(r) => assert!(r.contains("stop it first")),
            _ => panic!("rm on a stopping row must refuse"),
        }
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap(); // -> stopped
        assert!(matches!(
            begin_external_rm("deadbeef").unwrap(),
            LifecycleCas::Committed { generation: 2 }
        ));
    }

    #[test]
    fn complete_external_ignores_a_stale_generation() {
        // A stale retry's late completion (older generation) must never overwrite
        // the newer action - the core anti-clobber invariant.
        let _s = Scratch::new("stale-gen");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen 1, stopping
        complete_external("deadbeef", 1, ExternalState::Stopping, false, None).unwrap(); // -> failed
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen 2, stopping
                                                                 // A gen-1 completion arriving late is ignored; the gen-2 stopping stands.
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopping);
        assert_eq!(lc("deadbeef").unwrap().generation, 2);
    }

    #[test]
    fn complete_rm_deletes_on_ok_and_retains_on_err() {
        let _s = Scratch::new("complete-rm");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap();
        complete_external("deadbeef", 1, ExternalState::Stopping, true, None).unwrap(); // stopped
        begin_external_rm("deadbeef").unwrap(); // gen 2 removing
                                                // Failure keeps the tombstone stopped (rm stays retryable).
        complete_external(
            "deadbeef",
            2,
            ExternalState::Removing,
            false,
            Some("nope".into()),
        )
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopped);
        begin_external_rm("deadbeef").unwrap(); // gen 3 removing
        complete_external("deadbeef", 3, ExternalState::Removing, true, None).unwrap();
        assert!(
            lc("deadbeef").is_none(),
            "a successful rm deletes the tombstone"
        );
    }

    #[test]
    fn reconcile_lifecycle_leaves_a_generation_advanced_record_untouched() {
        // Lost-update guard (code review): a record a concurrent operator action
        // advanced PAST the reconcile's baseline generation is excluded from the
        // reconcile and left untouched - reconciling it against a pre-action
        // liveness snapshot would drop the action's completion.
        let _s = Scratch::new("reconcile-gen-guard");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen1 Stopping
        complete_external("deadbeef", 1, ExternalState::Stopping, false, None).unwrap(); // gen1 Failed
        let baseline: std::collections::HashMap<String, u64> =
            [("deadbeef".to_string(), 1u64)].into_iter().collect();
        // A concurrent retry advances the record to gen2 BEFORE reconcile applies.
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen2 Stopping
        let notices = reconcile_lifecycle(&baseline, |recs| {
            let n = recs.len();
            let mapped = recs
                .into_iter()
                .map(|mut r| {
                    r.state = ExternalState::Stopped;
                    r
                })
                .collect();
            (mapped, (0..n).map(|_| "reconciled".to_string()).collect())
        })
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopping);
        assert_eq!(lc("deadbeef").unwrap().generation, 2);
        assert!(
            notices.is_empty(),
            "the advanced record was excluded from reconcile"
        );
    }

    #[test]
    fn reconcile_lifecycle_applies_to_a_baseline_matching_record() {
        // The other half: with no concurrent action, a baseline-matching record
        // IS reconciled and its notices flow out.
        let _s = Scratch::new("reconcile-applies");
        begin_external_stop("deadbeef", "ext", "/tmp").unwrap(); // gen1 Stopping
        complete_external("deadbeef", 1, ExternalState::Stopping, false, None).unwrap(); // gen1 Failed
        let baseline: std::collections::HashMap<String, u64> =
            [("deadbeef".to_string(), 1u64)].into_iter().collect();
        let notices = reconcile_lifecycle(&baseline, |recs| {
            let mapped = recs
                .into_iter()
                .map(|mut r| {
                    r.state = ExternalState::Stopped;
                    r
                })
                .collect();
            (mapped, vec!["done".to_string()])
        })
        .unwrap();
        assert_eq!(lc("deadbeef").unwrap().state, ExternalState::Stopped);
        assert_eq!(notices, vec!["done".to_string()]);
    }

    #[test]
    fn load_drops_a_malformed_lifecycle_attach_id() {
        // Boundaries: a malformed attach_id never survives load, so a reconcile
        // or rm can never shell it.
        let s = Scratch::new("bad-lifecycle-id");
        let file = StoreFile {
            version: STORE_VERSION,
            external_lifecycle: vec![
                ExternalLifecycle {
                    attach_id: "deadbeef".into(),
                    name: "good".into(),
                    cwd: "/tmp".into(),
                    state: ExternalState::Stopped,
                    generation: 1,
                    updated_at: String::new(),
                    reason: None,
                },
                ExternalLifecycle {
                    attach_id: "; rm -rf".into(), // shell metachar
                    name: "evil".into(),
                    cwd: "/tmp".into(),
                    state: ExternalState::Stopped,
                    generation: 1,
                    updated_at: String::new(),
                    reason: None,
                },
            ],
            ..StoreFile::default()
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.external_lifecycle.len(), 1);
        assert_eq!(loaded.external_lifecycle[0].attach_id, "deadbeef");
        assert!(loaded
            .notice
            .as_deref()
            .unwrap()
            .contains("lifecycle record"));
    }
}
