//! Shared state files (Wave 3): `registry.json` (schema v4) and per-agent
//! `state.json` (schema v1), plus the flock-protected, atomic read/modify/write
//! helpers the daemon and worker share.
//!
//! Coupling-discipline invariants honored here:
//!
//! - **One writer per file via advisory lock.** Mutations take `LOCK_EX`; the
//!   daemon-down read path takes `LOCK_SH`. std's `File::lock`/`lock_shared`
//!   (stable since Rust 1.89) wrap `flock(2)`, the same advisory-lock family
//!   Python's `fcntl.flock` uses, so a Python `fno` process and the Rust daemon
//!   serialize against each other (US6.12, the load-bearing cross-language
//!   coupling proven by `tests/flock_interop.rs`).
//! - **Atomic publish via tempfile + rename.** A reader never observes a torn
//!   write; it sees either the old file or the fully-written new one. Optional
//!   fields are preserved across updates by round-tripping through the typed
//!   struct (no field-dropping reserialization).
//! - **`state.status` is canonical; `registry.status` is a projection** (LD10).
//!   This module stores both; conflict resolution (state wins) is the daemon's.

use crate::AgentStatus;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Current registry schema version.
///
/// v4 (ab-a171ceb2) is a forward-compat bump for `host_mode`: v4 is
/// structurally identical to v3 (host_mode is additive-optional and read
/// version-independently via absent==exec coercion), but stamping v4 forces a
/// pre-host_mode reader - which accepts only {1,2,3} and has no host_mode code
/// - to REJECT the store rather than silently treat an interactive row as exec
/// and orphan a live TUI during reconcile. Readers stay backward-compatible:
/// the accepted-version set still spans 1..=4 (see ACCEPTED_SCHEMA_VERSIONS in
/// client_verbs.rs and the Python load_registry range check).
pub const REGISTRY_SCHEMA_VERSION: u32 = 4;
/// Current per-agent state schema version (design: schema v1).
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Errors from state-file access.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("state json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "registry schema_version {found} unsupported; this fno understands 1..={max}. \
         Upgrade or downgrade fno to match."
    )]
    UnsupportedSchemaVersion { found: u32, max: u32 },
}

/// The daemon-owned agent registry (`~/.fno/agents/registry.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Registry {
    pub schema_version: u32,
    /// Rows. Python's `registry.write_registry` (cli/.../agents/registry.py)
    /// stores these under the canonical top-level `"agents"` key and reads ONLY
    /// that key (no `entries` fallback). Serialize under `agents` so a Rust write
    /// verb (`rm`/`stop`/reconcile) that rewrites a Python-authored registry
    /// leaves it readable by Python rather than stranding the surviving rows
    /// under an `entries` key Python ignores (Codex P1, PR #364). `alias =
    /// "entries"` keeps reading older daemon-written registries. Combined with
    /// ab-e5a57efa this makes the typed read path parse Python registries.
    #[serde(default, rename = "agents", alias = "entries")]
    pub entries: Vec<RegistryEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            schema_version: REGISTRY_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl Registry {
    /// Find an entry by agent name.
    pub fn find(&self, name: &str) -> Option<&RegistryEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Mutable find by agent name.
    pub fn find_mut(&mut self, name: &str) -> Option<&mut RegistryEntry> {
        self.entries.iter_mut().find(|e| e.name == name)
    }
}

/// One registry row (design schema v4). Optional fields default to `None` and
/// are preserved across `update_registry` because the whole row round-trips
/// through this typed struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryEntry {
    pub name: String,
    /// Daemon-set PTY field. Python's `AgentEntry` now mirrors it as
    /// `short_id: str = ""` (ab-b946b59c) so a real PTY row in a mixed registry
    /// is Python-readable and round-trips losslessly; `skip_serializing_if`
    /// still drops it when empty so a *Rust*-authored exec/ask row stays slim and
    /// a round-tripped Python row omits it (default-to-empty on read, ab-e5a57efa;
    /// Codex P1, PR #364). A real daemon PTY agent always has a non-empty
    /// short_id, so it still serializes for those rows; conversely a one-shot
    /// `ask` row always has an empty short_id (no worker-socket identity). That
    /// exclusivity is what [`RegistryEntry::is_one_shot_ask`] keys on -- a
    /// non-empty short_id on an ask row, or an empty one on a PTY row, is a
    /// producer bug. (Python mirrors with a `str` default, not `Option`, because
    /// a `"short_id": null` would fail this `String` field's deserialize.)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub short_id: String,
    pub provider: String,
    pub cwd: String,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` as
    /// `project_root: str = ""` (ab-b946b59c; see `short_id`): default on read,
    /// skip-when-empty on write.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_root: String,
    /// On disk this is Rust-set only (Python's `session_id` is a computed
    /// `@property`, excluded from its serialized rows): skip when absent so
    /// Python can read a Rust-written row (Codex P1). When a Rust PTY row DOES
    /// record one, Python's load_registry drops the key before constructing the
    /// entry and recomputes the same projection from the *_session_id fields
    /// (ab-b946b59c).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub claude_short_id: Option<String>,
    /// The FULL claude session UUID -- the stream-json `--resume` target,
    /// distinct from the 8-hex `claude_short_id`/jobId (a 32-bit prefix, not
    /// collision-proof as a resume key). Shared field with Python's `AgentEntry`
    /// (`#[serde(default)]`, always emitted as null when absent, matching the
    /// sibling provider-id fields), so a row round-trips between the two
    /// languages. The daemon reads it to build the resume argv for the
    /// stream-json host lane. [stream-json host lane node]
    #[serde(default)]
    pub claude_session_uuid: Option<String>,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` (ab-b946b59c):
    /// skip when absent (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messaging_socket_path: Option<String>,
    #[serde(default)]
    pub codex_session_id: Option<String>,
    #[serde(default)]
    pub gemini_session_id: Option<String>,
    #[serde(default)]
    pub mcp_channel_id: Option<String>,
    /// Hosting mode: absent/`None` == `"exec"` (one-shot, the default for every
    /// pre-existing row), `Some("interactive")` == a long-lived drivable TUI
    /// (`fno agents host`/`promote`). Skip-when-`None` so a *Rust*-authored exec
    /// row omits the key; Python's missing-key coercion then maps the absence
    /// back to `"exec"`. (Python itself always emits the key via `asdict` -- as
    /// `"exec"` or `"interactive"` -- and Rust reads the concrete value fine, so
    /// both directions agree.) Consumers must read it via
    /// [`RegistryEntry::host_mode_or_default`], never the raw `Option`, so the
    /// absent==exec rule lives in one place. [interactive-drive node]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_mode: Option<String>,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` (ab-b946b59c):
    /// skip when absent (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_session_id: Option<String>,
    pub status: AgentStatus,
    #[serde(default)]
    pub last_message_at: Option<String>,
    pub created_at: String,
    /// Daemon-set PTY field, mirrored in Python's `AgentEntry` as
    /// `pid: Optional[int]` (ab-b946b59c): skip when absent so a round-tripped
    /// Python row stays slim and Python-readable (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The worker process's start time, captured alongside `pid` at spawn, used
    /// to detect PID reuse: a liveness/reap/signal decision treats `pid` as "our
    /// worker" only if the live process's start time still matches this
    /// (ab-d19e6458). Per-host, per-boot value (Linux: `/proc/<pid>/stat` field
    /// 22 in clock ticks; macOS: `kinfo_proc` start `timeval` in microseconds) —
    /// only ever compared for equality against a fresh read of the SAME pid, so
    /// the unit/epoch difference across platforms is irrelevant. Daemon-set PTY
    /// field, mirrored in Python's `AgentEntry` (ab-b946b59c); skip when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    #[serde(default)]
    pub log_path: Option<String>,
    /// Timestamp of the most recent reconcile probe (finding #1 High): the
    /// reconcile sweep orders entries by ASC `last_reconciled_at` so a
    /// budget-exhausted sweep stays fair across a large registry. Daemon-set,
    /// mirrored in Python's `AgentEntry` (ab-b946b59c); skip when absent (Codex P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<String>,
}

/// `host_mode` value for a one-shot exec session (the default when absent).
pub const HOST_MODE_EXEC: &str = "exec";
/// `host_mode` value for a long-lived drivable interactive session.
pub const HOST_MODE_INTERACTIVE: &str = "interactive";

/// Claude spawn `mode` (D2, inside-out-multiplexer E1). Disambiguates the two
/// claude PTY lanes WITHIN an interactive `host_mode`: `stream_json` is the
/// Agent-SDK adoption lane (`claude -p --resume`, billed against the SDK pool);
/// `interactive` is the subscription-billed `ClaudeProvider` PTY lane (the
/// keystone). Absent reads as `stream_json` so every existing promote call site
/// keeps its current behavior; grid/relay request `interactive` explicitly. The
/// daemon routes on this field, never on a guess.
pub const CLAUDE_MODE_STREAM_JSON: &str = "stream_json";
/// See [`CLAUDE_MODE_STREAM_JSON`]: the interactive subscription-billed lane.
pub const CLAUDE_MODE_INTERACTIVE: &str = "interactive";

impl RegistryEntry {
    /// The hosting mode with the absent==exec rule applied in one place.
    /// `None` on disk (and the legacy rows that predate the field) read as
    /// [`HOST_MODE_EXEC`]; an explicit value passes through. Reconcile/liveness
    /// and the spawn path must use this, never the raw `Option`, so a missing
    /// key can never be mistaken for a non-exec mode. [interactive-drive node]
    pub fn host_mode_or_default(&self) -> &str {
        self.host_mode.as_deref().unwrap_or(HOST_MODE_EXEC)
    }

    /// True when this row is a long-lived interactive host (vs a one-shot exec
    /// session). The reconcile branch keys off this: an exec worker that exited
    /// is normal; an interactive worker is expected to stay live until `/quit`.
    pub fn is_interactive(&self) -> bool {
        self.host_mode_or_default() == HOST_MODE_INTERACTIVE
    }

    /// True when this row is a one-shot `ask` agent the daemon does NOT manage as
    /// a worker process: empty `short_id` (no worker-socket identity) AND no
    /// recorded `pid`. Such an agent has no process whose liveness could make it
    /// `live` -- its terminal status is `exited`, and its post-run value is
    /// *resumability* (a recorded provider session id), surfaced separately from
    /// status via the `session_id` projection. Only PTY agents (`spawn`/`host`/
    /// `promote`) carry a non-empty short_id + pid and can be `live`; this is the
    /// invariant documented on the `short_id` field ("a real daemon PTY agent
    /// always has a non-empty short_id"). Reconcile uses this to settle a
    /// finished ask to `exited` by process-liveness alone, never consulting
    /// session-file reachability for status. [plan ab-70faa65b, Locked Decision #1]
    pub fn is_one_shot_ask(&self) -> bool {
        self.short_id.is_empty() && self.pid.is_none()
    }
}

/// Per-agent runtime state (`<short_id>/state.json`, schema v1). `state.status`
/// is canonical (LD10).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentState {
    pub schema_version: u32,
    pub short_id: String,
    pub status: AgentStatus,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub last_message_at: Option<String>,
    #[serde(default)]
    pub last_reply: Option<String>,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub last_restart_at: Option<String>,
    /// `None` for shellout (claude) agents; `Some` for PTY-managed agents.
    #[serde(default)]
    pub pty: Option<PtyState>,
}

impl AgentState {
    /// Construct a fresh PTY-managed agent state.
    pub fn new_pty(short_id: impl Into<String>) -> Self {
        AgentState {
            schema_version: STATE_SCHEMA_VERSION,
            short_id: short_id.into(),
            status: AgentStatus::Spawning,
            ready: false,
            last_message_at: None,
            last_reply: None,
            restart_count: 0,
            last_restart_at: None,
            pty: Some(PtyState::default()),
        }
    }
}

/// An open interactive drive window. Bundling the drive facts behind a single
/// `Option<DriveWindow>` makes the inconsistent `{drive_active: false,
/// drive_session_id: Some(..)}` state impossible: either there is a window
/// (`Some`) carrying all its fields, or there is none (`None`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DriveWindow {
    pub session_id: Option<String>,
    pub mode: Option<String>,
    /// Monotonic-clock baseline of the last drive heartbeat (count-during-sleep
    /// ns; see [`crate::MonotonicTimestamp`]).
    pub last_heartbeat_at_monotonic_ns: Option<u64>,
}

/// PTY sub-state. The on-disk shape stays flat (`active`, `drive_active`,
/// `drive_session_id`, `drive_mode`, `last_heartbeat_at_monotonic_ns`) via a
/// hand-written serde impl below, so cross-language schema parity (Wave 7) is a
/// direct field map; in memory the drive cluster is one `Option<DriveWindow>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PtyState {
    pub active: bool,
    /// `Some` while an interactive drive window is open; `None` otherwise.
    pub drive: Option<DriveWindow>,
}

impl PtyState {
    /// Recovery step 4/5 ordering primitive (finding #12 Critical): atomically
    /// READ the active drive window (returning its session id + mode + last
    /// heartbeat) AND clear it. Callers MUST use the returned value to emit
    /// `drive_crashed` — the read happens here, before the clear, so the event
    /// reflects what the window was. Returns `None` if no drive was active.
    ///
    /// With the drive cluster behind one `Option`, read-then-clear is just
    /// `Option::take`: there is no window between the read and the clear for a
    /// second observer to see a half-cleared state.
    pub fn take_active_drive(&mut self) -> Option<DriveWindow> {
        self.drive.take()
    }
}

/// Flat on-disk projection of [`PtyState`], mediating between the typed
/// `Option<DriveWindow>` and the design's flat `state.json` schema. `drive_active`
/// is the discriminant; the option fields default to `None`/absent.
#[derive(Serialize, Deserialize)]
struct PtyStateWire {
    active: bool,
    #[serde(default)]
    drive_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_heartbeat_at_monotonic_ns: Option<u64>,
}

impl Serialize for PtyState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let wire = match &self.drive {
            Some(d) => PtyStateWire {
                active: self.active,
                drive_active: true,
                drive_session_id: d.session_id.clone(),
                drive_mode: d.mode.clone(),
                last_heartbeat_at_monotonic_ns: d.last_heartbeat_at_monotonic_ns,
            },
            None => PtyStateWire {
                active: self.active,
                drive_active: false,
                drive_session_id: None,
                drive_mode: None,
                last_heartbeat_at_monotonic_ns: None,
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PtyState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = PtyStateWire::deserialize(deserializer)?;
        // `drive_active` is canonical for window presence. A legacy/partial file
        // with the flag clear collapses any stray option fields to `None`, which
        // is exactly the inconsistent state the refactor makes unrepresentable.
        let drive = if wire.drive_active {
            Some(DriveWindow {
                session_id: wire.drive_session_id,
                mode: wire.drive_mode,
                last_heartbeat_at_monotonic_ns: wire.last_heartbeat_at_monotonic_ns,
            })
        } else {
            None
        };
        Ok(PtyState {
            active: wire.active,
            drive,
        })
    }
}

// ---------------------------------------------------------------------------
// Locked, atomic file access.
// ---------------------------------------------------------------------------

/// Load the registry under a shared lock. A missing file yields an empty
/// registry (0 agents is a valid steady state, not an error). The shared lock
/// is the daemon-down read path (`fno agents list` when the socket is down)
/// AND recovery step 1.
pub fn load_registry(path: &Path) -> Result<Registry, StateError> {
    // Lock the SAME sidecar `update_registry` locks (shared mode here), not the
    // data file. This is the canonical cross-language lock target: a Python
    // `fno` writer taking `flock` on `<registry>.lock` and the Rust daemon's
    // exclusive write-lock then live in one domain, so reader/writer and
    // cross-language writers actually mutually exclude (US6.12). Locking the
    // data file directly would (a) not exclude against the sidecar-based
    // writer and (b) reintroduce the rename-invalidates-fd footgun.
    // Acquire the lock FIRST, then decide existence: a `!path.exists()` check
    // before the lock could race a concurrent writer creating registry.json and
    // return a stale empty registry (Codex P2). The open-after-lock below is the
    // authoritative existence check.
    let lock = acquire_shared(&lock_path(path))?;
    let result = match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_registry_tolerant(&file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = lock.unlock();
            return Ok(Registry::default());
        }
        Err(e) => {
            let _ = lock.unlock();
            return Err(e.into());
        }
    };
    let _ = lock.unlock();
    result
}

/// Read a registry, tolerating ONLY a genuinely empty file (0 bytes / all
/// whitespace) as the empty registry. A present-but-unparseable file (malformed
/// JSON, schema mismatch, corruption) propagates `StateError::Json` instead of
/// silently defaulting: a default fed back through `update_registry`'s
/// read-modify-write would publish an empty registry and permanently wipe every
/// other agent (Gemini high, PR #364). `write_json_atomic` publishes via
/// tempfile + rename, so a reader never observes a torn write -- a parse failure
/// is therefore real corruption, not the transient partial read the prior
/// `unwrap_or_default()` was excusing.
fn read_registry_tolerant(mut file: &File) -> Result<Registry, StateError> {
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(Registry::default());
    }
    let reg: Registry = serde_json::from_str(&buf)?;
    // Forward-compat guard on the TYPED daemon path (Codex P2, ab-a171ceb2):
    // the raw client path (client_verbs::load_registry_entries) already rejects
    // unsupported versions, but the daemon reads through here and previously
    // accepted any u32. Reject anything outside 1..=REGISTRY_SCHEMA_VERSION so a
    // pre-host_mode daemon refuses a v4 store (instead of treating an interactive
    // row as exec) and the current daemon refuses a future v5 store.
    if reg.schema_version < 1 || reg.schema_version > REGISTRY_SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion {
            found: reg.schema_version,
            max: REGISTRY_SCHEMA_VERSION,
        });
    }
    Ok(reg)
}

/// Read-modify-write the registry under an exclusive lock, publishing the
/// result atomically (tempfile + rename). The lock is held across the whole
/// read-modify-write so two daemons (or a daemon and a Python `fno`) never
/// interleave. The closure mutates the registry in place.
pub fn update_registry<F, T>(path: &Path, f: F) -> Result<T, StateError>
where
    F: FnOnce(&mut Registry) -> T,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Lock on a stable sidecar so the rename of the data file never invalidates
    // the lock fd (renaming the locked file out from under a held flock is the
    // classic footgun; locking the sidecar sidesteps it entirely).
    let lock = acquire_exclusive(&lock_path(path))?;
    let mut registry = read_existing_registry(path)?;
    let out = f(&mut registry);
    // Upgrade-on-write (Codex P2, ab-a171ceb2): stamp the current schema version
    // so a Rust write of an older (e.g. v3) store bumps it to v4, matching
    // Python's write_registry (which always writes SCHEMA_VERSION). Without this,
    // adding host_mode to an existing v3 registry would leave schema_version:3 and
    // a pre-host_mode reader would still accept it - defeating the forward-compat
    // bump for every store that predates it (the common case).
    registry.schema_version = REGISTRY_SCHEMA_VERSION;
    write_json_atomic(path, &registry)?;
    let _ = lock.unlock();
    Ok(out)
}

fn read_existing_registry(path: &Path) -> Result<Registry, StateError> {
    match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_registry_tolerant(&file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e.into()),
    }
}

/// Load a per-agent `state.json`. `Ok(None)` when the file is absent (recovery
/// distinguishes "registry entry without state.json" from a present-but-partial
/// state).
pub fn load_state(path: &Path) -> Result<Option<AgentState>, StateError> {
    // Lock the SAME `.lock` sidecar `write_state_atomic` locks (shared mode),
    // not the data file: readers and writers must synchronize on one inode or
    // a read can race a concurrent write/rename (Codex P1). Acquire the lock
    // BEFORE deciding existence so a writer creating the file mid-call cannot
    // be missed.
    let lock = acquire_shared(&lock_path(path))?;
    let r = match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_json::<AgentState>(&file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = lock.unlock();
            return Ok(None);
        }
        Err(e) => {
            let _ = lock.unlock();
            return Err(e.into());
        }
    };
    let _ = lock.unlock();
    match r {
        Ok(s) => Ok(Some(s)),
        // Present but empty/partial: treat as absent state so recovery marks
        // the agent inconsistent rather than crashing.
        Err(_) => Ok(None),
    }
}

/// Atomically write a per-agent `state.json` (tempfile + rename) under an
/// exclusive lock on its sidecar.
pub fn write_state_atomic(path: &Path, state: &AgentState) -> Result<(), StateError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = acquire_exclusive(&lock_path(path))?;
    write_json_atomic(path, state)?;
    let _ = lock.unlock();
    Ok(())
}

/// Read-modify-write a per-agent `state.json` while holding the exclusive
/// sidecar lock across the WHOLE operation, so concurrent writers cannot
/// interleave between the read and the write (the lost-update footgun a
/// `load_state` + `write_state_atomic` pair has).
///
/// Returns `Ok(false)` without calling `f` when the file is absent or partial:
/// drive window mutations must never fabricate a `state.json` on the worker's
/// behalf (recovery distinguishes "registry entry without state.json"). The
/// drive admit / cleanup paths route their window writes through here so a
/// stale-driver takeover cannot drop the authority window via a read that
/// predates the new driver's write.
pub fn update_state_atomic<F>(path: &Path, f: F) -> Result<bool, StateError>
where
    F: FnOnce(&mut AgentState),
{
    let lock = acquire_exclusive(&lock_path(path))?;
    let existing = match OpenOptions::new().read(true).open(path) {
        Ok(file) => read_json::<AgentState>(&file).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            let _ = lock.unlock();
            return Err(e.into());
        }
    };
    let result = match existing {
        Some(mut st) => {
            f(&mut st);
            write_json_atomic(path, &st)?;
            true
        }
        None => false,
    };
    let _ = lock.unlock();
    Ok(result)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Open (creating if needed) the lock sidecar and take an exclusive advisory
/// lock, blocking until acquired. The returned `File` holds the lock until it
/// is unlocked or dropped.
fn acquire_exclusive(lock_file: &Path) -> Result<File, StateError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)?;
    file.lock()?;
    Ok(file)
}

/// Open (creating if needed) the lock sidecar and take a shared advisory lock,
/// blocking until acquired. Multiple readers share; an exclusive writer
/// excludes them. Same sidecar target as [`acquire_exclusive`].
fn acquire_shared(lock_file: &Path) -> Result<File, StateError> {
    if let Some(parent) = lock_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)?;
    file.lock_shared()?;
    Ok(file)
}

fn read_json<T: for<'de> Deserialize<'de>>(mut file: &File) -> Result<T, StateError> {
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StateError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        let bytes = serde_json::to_vec_pretty(value)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-state-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_entry(name: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: format!("{name}-id"),
            provider: "codex".into(),
            cwd: "/tmp/x".into(),
            project_root: "/tmp/x".into(),
            session_id: Some("uuid-1".into()),
            claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: Some("uuid-1".into()),
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            pid: Some(1234),
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
        }
    }

    #[test]
    fn missing_registry_loads_empty() {
        let dir = tmpdir("missing");
        let reg = load_registry(&dir.join("registry.json")).unwrap();
        assert_eq!(reg.schema_version, REGISTRY_SCHEMA_VERSION);
        assert!(reg.entries.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn python_written_registry_loads_via_typed_path() {
        // Regression for ab-e5a57efa: the typed daemon read path
        // (`load_registry`, used by list/stop/rm/reconcile/status) must parse a
        // registry authored by Python's `registry.write_registry`. That writer
        // uses the top-level `"agents"` key and `AgentEntry` rows that omit the
        // Rust-daemon-only `short_id`/`project_root` fields. Before the fix the
        // whole-file parse failed and `unwrap_or_default()` returned 0 agents.
        let dir = tmpdir("python-registry");
        let path = dir.join("registry.json");
        // Byte-for-byte the shape Python emits (no short_id, no project_root,
        // key is "agents").
        let python_json = r#"{
  "schema_version": 3,
  "agents": [
    {
      "name": "worker-claude",
      "provider": "claude",
      "cwd": "/Users/x/proj",
      "log_path": "/Users/x/.fno/agents/worker-claude.log",
      "claude_short_id": "abc123",
      "codex_session_id": null,
      "gemini_session_id": null,
      "created_at": "2026-05-26T00:00:00Z",
      "status": "live",
      "last_message_at": null,
      "mcp_channel_id": null
    }
  ]
}"#;
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, python_json).unwrap();

        let reg = load_registry(&path).unwrap();
        assert_eq!(reg.entries.len(), 1, "Python-written row must be read");
        let e = reg.find("worker-claude").unwrap();
        assert_eq!(e.provider, "claude");
        assert_eq!(e.status, AgentStatus::Live);
        assert_eq!(e.claude_short_id.as_deref(), Some("abc123"));
        // Rust-only fields default to empty for Python-authored rows.
        assert_eq!(e.short_id, "");
        assert_eq!(e.project_root, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn python_row_roundtrips_to_python_shape_under_agents_key() {
        // Codex P1 (PR #364): after the daemon rewrites a Python-authored
        // registry (e.g. `rm` removing one agent), the surviving rows must stay
        // readable by Python -- which reads ONLY the top-level `agents` key and
        // whose `AgentEntry(**row)` rejects unknown keys. So the serialized form
        // must (a) use `agents`, not `entries`, and (b) omit every Rust-only
        // field that a Python row lacks (short_id/project_root/session_id/
        // messaging_socket_path/cc_session_id/pid/last_reconciled_at).
        let python_json = r#"{"schema_version":3,"agents":[
            {"name":"w","provider":"codex","cwd":"/p","log_path":"/l",
             "claude_short_id":null,"codex_session_id":"sid","gemini_session_id":null,
             "created_at":"2026-05-26T00:00:00Z","status":"live","last_message_at":null,
             "mcp_channel_id":null}]}"#;
        let reg: Registry = serde_json::from_str(python_json).unwrap();
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();

        assert!(out.get("agents").is_some(), "must serialize under `agents`");
        assert!(out.get("entries").is_none(), "must NOT serialize `entries`");
        let row = &out["agents"][0];
        for rust_only in [
            "short_id",
            "project_root",
            "session_id",
            "messaging_socket_path",
            "cc_session_id",
            "pid",
            "pid_start_time",
            "last_reconciled_at",
        ] {
            assert!(
                row.get(rust_only).is_none(),
                "Python-authored row must omit Rust-only field `{rust_only}`"
            );
        }
        // Python's known fields survive.
        assert_eq!(row["name"], "w");
        assert_eq!(row["codex_session_id"], "sid");
    }

    #[test]
    fn host_mode_cross_language_round_trip_parity() {
        // interactive-drive node (ab-26b5fe82): the host_mode add must round-trip
        // both directions across the Rust<->Python registry boundary.

        // (a) Rust READS a Python-written row that OMITS host_mode -> exec.
        let no_key = r#"{"schema_version":3,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
        let reg: Registry = serde_json::from_str(no_key).unwrap();
        assert_eq!(reg.entries[0].host_mode, None);
        assert_eq!(reg.entries[0].host_mode_or_default(), HOST_MODE_EXEC);
        assert!(!reg.entries[0].is_interactive());

        // (b) Rust READS a row carrying host_mode="interactive" -> interactive.
        let interactive = r#"{"schema_version":3,"agents":[
            {"name":"bot2","provider":"codex","cwd":"/p","log_path":"/l",
             "codex_session_id":"019e7157","created_at":"2026-05-26T00:00:00Z",
             "status":"live","host_mode":"interactive"}]}"#;
        let reg: Registry = serde_json::from_str(interactive).unwrap();
        assert_eq!(reg.entries[0].host_mode_or_default(), HOST_MODE_INTERACTIVE);
        assert!(reg.entries[0].is_interactive());

        // (c) Rust WRITES an exec row (host_mode None) -> key OMITTED, so a
        // Python AgentEntry(**row) does not gain an unexpected key and Python's
        // missing-key coercion maps the absence back to "exec".
        let mut exec_entry = sample_entry("w");
        exec_entry.host_mode = None;
        let mut reg = Registry::default();
        reg.entries.push(exec_entry);
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert!(
            out["agents"][0].get("host_mode").is_none(),
            "exec row must omit host_mode (skip_serializing_if)"
        );

        // (d) Rust WRITES an interactive row -> host_mode present and readable.
        let mut int_entry = sample_entry("bot2");
        int_entry.host_mode = Some(HOST_MODE_INTERACTIVE.to_string());
        let mut reg = Registry::default();
        reg.entries.push(int_entry);
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(out["agents"][0]["host_mode"], "interactive");
    }

    #[test]
    fn rust_reads_python_row_with_explicit_empty_and_null_fields() {
        // ab-b946b59c: Python's `AgentEntry` now mirrors the Rust-only PTY
        // fields, so its `asdict` emits them for EVERY row -- short_id/
        // project_root as "" (their Rust type is `String`, so a null would fail
        // deserialize) and the Option fields as null. Rust must read that shape.
        let python_json = r#"{"schema_version":4,"agents":[
            {"name":"py-ask","provider":"codex","cwd":"/p","log_path":"/l",
             "short_id":"","project_root":"",
             "claude_short_id":null,"codex_session_id":"sid","gemini_session_id":null,
             "claude_session_uuid":null,"messaging_socket_path":null,"cc_session_id":null,
             "mcp_channel_id":null,"host_mode":"exec",
             "created_at":"2026-05-26T00:00:00Z","status":"exited","last_message_at":null,
             "pid":null,"pid_start_time":null,"last_reconciled_at":null}]}"#;
        let reg: Registry = serde_json::from_str(python_json).unwrap();
        let e = &reg.entries[0];
        assert_eq!(e.name, "py-ask");
        assert_eq!(e.short_id, ""); // "" deserializes into the String field
        assert_eq!(e.project_root, "");
        assert_eq!(e.pid, None); // null -> None for the Option fields
        assert_eq!(e.pid_start_time, None);
        assert_eq!(e.cc_session_id, None);
        assert_eq!(e.codex_session_id.as_deref(), Some("sid"));
        assert!(e.is_one_shot_ask(), "empty short_id + no pid => ask row");
    }

    #[test]
    fn pty_agent_still_serializes_its_short_id() {
        // The skip-when-empty must NOT drop a real daemon agent's short_id/pid.
        let mut reg = Registry::default();
        reg.entries.push(sample_entry("worker-A")); // short_id "worker-A-id", pid Some
        let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
        let row = &out["agents"][0];
        assert_eq!(row["short_id"], "worker-A-id");
        assert_eq!(row["pid"], 1234);
    }

    #[test]
    fn empty_registry_file_loads_default_but_corrupt_file_errors() {
        // Gemini high (PR #364): an empty/whitespace file is a valid empty
        // registry, but a present-but-unparseable file must error LOUDLY rather
        // than default -- otherwise update_registry's read-modify-write republishes
        // the empty default and wipes every other agent.
        let dir = tmpdir("corrupt-registry");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");

        // Empty file -> empty registry, no error.
        std::fs::write(&path, "   \n").unwrap();
        assert!(load_registry(&path).unwrap().entries.is_empty());

        // Corrupt (non-empty, unparseable) file -> error, not silent default.
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(
            load_registry(&path).is_err(),
            "corrupt registry must surface an error"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_refuses_to_wipe_a_corrupt_registry() {
        // The data-loss path Gemini flagged: update_registry reads, mutates,
        // writes. If the read silently defaulted on a corrupt file, the write
        // would publish an (almost) empty registry. It must instead propagate the
        // parse error and leave the file byte-for-byte intact.
        let dir = tmpdir("no-wipe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        let corrupt = "{\"schema_version\": 3, \"agents\": [ BROKEN";
        std::fs::write(&path, corrupt).unwrap();

        let result = update_registry(&path, |r| r.entries.push(sample_entry("new-A")));
        assert!(result.is_err(), "update over corrupt registry must error");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            corrupt,
            "corrupt registry must be left untouched, not overwritten"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_registry_upgrades_schema_version_on_write() {
        // Codex P2 (ab-a171ceb2): a Rust write of an existing older store must
        // bump schema_version to the current version, or the forward-compat bump
        // never takes effect for the common case (stores that predate it).
        let dir = tmpdir("upgrade-on-write");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(
            &path,
            r#"{"schema_version":3,"agents":[{"name":"w","provider":"codex","cwd":"/p","log_path":"/l","created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#,
        )
        .unwrap();
        update_registry(&path, |r| r.entries.push(sample_entry("w2"))).unwrap();
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk["schema_version"], REGISTRY_SCHEMA_VERSION,
            "Rust write must upgrade the on-disk schema_version"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_registry_rejects_unsupported_schema_version() {
        // Codex P2 (ab-a171ceb2): the typed daemon read path must reject a version
        // outside 1..=REGISTRY_SCHEMA_VERSION (a future v5, or - for an old daemon -
        // a v4 it cannot interpret), while v1..=v4 still read.
        let dir = tmpdir("version-guard");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(&path, r#"{"schema_version":5,"agents":[]}"#).unwrap();
        match load_registry(&path) {
            Err(StateError::UnsupportedSchemaVersion { found, max }) => {
                assert_eq!(found, 5);
                assert_eq!(max, REGISTRY_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
        std::fs::write(&path, r#"{"schema_version":1,"agents":[]}"#).unwrap();
        assert!(
            load_registry(&path).is_ok(),
            "v1 must still read (back-compat)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_then_load_roundtrips_and_preserves_optionals() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("registry.json");
        update_registry(&path, |r| r.entries.push(sample_entry("worker-A"))).unwrap();

        // A second update that only flips status must preserve codex_session_id.
        update_registry(&path, |r| {
            r.find_mut("worker-A").unwrap().status = AgentStatus::Idle;
        })
        .unwrap();

        let reg = load_registry(&path).unwrap();
        let e = reg.find("worker-A").unwrap();
        assert_eq!(e.status, AgentStatus::Idle);
        assert_eq!(e.codex_session_id.as_deref(), Some("uuid-1"));
        assert_eq!(e.pid, Some(1234));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_json_absent_is_none_present_roundtrips() {
        let dir = tmpdir("state");
        let path = dir.join("wkA/state.json");
        assert!(load_state(&path).unwrap().is_none());

        let st = AgentState::new_pty("wkA");
        write_state_atomic(&path, &st).unwrap();
        let back = load_state(&path).unwrap().unwrap();
        assert_eq!(back.short_id, "wkA");
        assert_eq!(back.status, AgentStatus::Spawning);
        assert!(back.pty.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_state_file_treated_as_absent() {
        // Recovery's "registry entry with partial state.json" path: a present
        // but empty file must read as None (-> inconsistent), never an error.
        let dir = tmpdir("empty-state");
        let path = dir.join("state.json");
        std::fs::write(&path, b"").unwrap();
        assert!(load_state(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn take_active_drive_reads_before_clear() {
        // The recovery ordering invariant in miniature: the returned value
        // carries the session id, and after the call the window is cleared.
        let mut pty = PtyState {
            active: true,
            drive: Some(DriveWindow {
                session_id: Some("drive-uuid".into()),
                mode: Some("interactive".into()),
                last_heartbeat_at_monotonic_ns: Some(42),
            }),
        };
        let taken = pty.take_active_drive().expect("a drive was active");
        assert_eq!(taken.session_id.as_deref(), Some("drive-uuid"));
        assert_eq!(taken.mode.as_deref(), Some("interactive"));
        // Cleared after read.
        assert!(pty.drive.is_none());
        // Idempotent: a second take finds nothing.
        assert!(pty.take_active_drive().is_none());
    }

    #[test]
    fn take_active_drive_none_when_no_drive() {
        let mut pty = PtyState::default();
        assert!(pty.take_active_drive().is_none());
    }

    #[test]
    fn pty_state_wire_shape_is_flat_and_stable() {
        // The Option<DriveWindow> in-memory shape must still serialize to the
        // flat state.json schema (Wave 7 cross-language parity).
        let no_drive = PtyState {
            active: true,
            drive: None,
        };
        assert_eq!(
            serde_json::to_value(&no_drive).unwrap(),
            serde_json::json!({"active": true, "drive_active": false})
        );

        let with_drive = PtyState {
            active: true,
            drive: Some(DriveWindow {
                session_id: Some("d-1".into()),
                mode: Some("interactive".into()),
                last_heartbeat_at_monotonic_ns: Some(99),
            }),
        };
        assert_eq!(
            serde_json::to_value(&with_drive).unwrap(),
            serde_json::json!({
                "active": true,
                "drive_active": true,
                "drive_session_id": "d-1",
                "drive_mode": "interactive",
                "last_heartbeat_at_monotonic_ns": 99
            })
        );
        // Roundtrips back to the same typed value.
        let back: PtyState =
            serde_json::from_value(serde_json::to_value(&with_drive).unwrap()).unwrap();
        assert_eq!(back, with_drive);
    }

    #[test]
    fn pty_state_collapses_inconsistent_legacy_shape() {
        // A legacy/partial file with drive_active:false but a stray session_id
        // deserializes to drive: None - the inconsistent state is normalized
        // away rather than carried.
        let legacy = serde_json::json!({
            "active": true,
            "drive_active": false,
            "drive_session_id": "stray",
        });
        let pty: PtyState = serde_json::from_value(legacy).unwrap();
        assert!(pty.drive.is_none());
    }
}
