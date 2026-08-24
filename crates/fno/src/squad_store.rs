//! Persisted named squads (`~/.fno/squads.json`): the durable half of the
//! session-scoped [`crate::squad::Squad`] model.
//!
//! Every squad persists (operator decision: any squad created remains across a
//! restart, TUI or API), not only explicit NAMED workspaces. Identity is `name`
//! when the squad is named, else a durable per-squad `key` minted on first
//! persist (see [`mint_key`] / [`same_squad`]) - NOT `origins`, because the
//! model permits two squads to share an origin, so origins would collide two
//! distinct unnamed squads (two cleared names, or two sessions' home for one
//! repo). A squad with neither a name nor a key has no identity and is the one
//! thing that does not persist. A member's PANE is still ephemeral (re-created
//! at restore, never stored); the store holds the identity, its origins (for
//! restore / owns_path), and its member attach-ids.
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

// A per-thread store path for tests, so a store-touching test never mutates the
// process-global environment (a `set_var` there would race any concurrent
// `getenv` in a sibling test - a real data race). Cargo runs each test on its
// own thread, so a thread-local gives every test full isolation with no lock.
// An explicit path may be installed for assertions; otherwise the test gets a
// unique temp path which is removed when its worker thread exits.
#[cfg(test)]
struct TestPath {
    explicit: Option<PathBuf>,
    fallback_dir: PathBuf,
}

#[cfg(test)]
impl TestPath {
    fn new() -> Self {
        let fallback_dir = std::env::temp_dir().join(format!(
            "fno-squadstore-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&fallback_dir);
        Self {
            explicit: None,
            fallback_dir,
        }
    }

    fn path(&self) -> PathBuf {
        self.explicit
            .clone()
            .unwrap_or_else(|| self.fallback_dir.join("squads.json"))
    }
}

#[cfg(test)]
impl Drop for TestPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.fallback_dir);
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PATH: std::cell::RefCell<TestPath> =
        std::cell::RefCell::new(TestPath::new());
}

/// Point this thread's store at `dir/squads.json` (test-only).
#[cfg(test)]
pub(crate) fn set_test_path(dir: &std::path::Path) {
    TEST_PATH.with(|c| c.borrow_mut().explicit = Some(dir.join("squads.json")));
}

/// Clear this thread's store override (test-only).
#[cfg(test)]
pub(crate) fn clear_test_path() {
    TEST_PATH.with(|c| c.borrow_mut().explicit = None);
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
    /// (x-5f7f) `#[serde(default)]` so a worker member can omit it: a
    /// non-claude worker pane has no claude jobId, and its identity is the
    /// `worker` registry name instead. Empty for those members; the load
    /// gate accepts either shape.
    #[serde(default)]
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
    /// (x-caef) The pane's spawn cwd at store time, re-derived fresh on every
    /// persist like `tab_name`. Restore spawns `claude attach` here instead of
    /// the squad's `origins[0]` when the directory still exists (a worktree
    /// worker's true home), falling back to `origins[0]` with a notice when it
    /// does not. `#[serde(default)]`, same no-quarantine rule as `tab_name`.
    #[serde(default)]
    pub cwd: Option<String>,
    /// (x-5f7f) The registry name of a non-claude worker pane - the JOIN key.
    /// Harness, harness session id, cwd and account live on the registry row,
    /// which is the file that already owns them; restore never respawns this
    /// member, it renders idle and resumes through the harness's own form.
    /// Empty `attach_id` + `Some(worker)` is the worker shape; a claude
    /// attach member keeps `attach_id` and leaves this `None`.
    /// `#[serde(default)]`, same no-quarantine rule as `tab_name`.
    #[serde(default)]
    pub worker: Option<String>,
}

/// A durable per-squad identity for an UNNAMED squad, minted the first time it
/// persists (a named squad keys by `name` and leaves this empty). 16 hex chars
/// from `/dev/urandom` - unique without a `rand`/`uuid` dependency. A read
/// failure falls back to a monotonic-ish stamp so a mint never blocks a persist.
pub fn mint_key() -> String {
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok()
    {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    format!("t{:x}", now_secs())
}

/// FNV-1a over bytes: deterministic and dependency-free, the same hash server's
/// `mission_sid` uses. File-local (this module does not import server.rs); kept
/// in sync with `crates/fno/src/server.rs::fnv1a`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// A durable identity for an UNNAMED squad derived from what it represents: its
/// sorted, deduped origin set (the home squad for a repo has one origin, so it
/// derives one key forever and every later persist upserts onto a single row
/// across unbounded restarts - x-e447). Order-independent and set-stable, so a
/// multi-origin lane derives consistently. The unit separator (`\x1f`, which
/// cannot occur in a path) keeps `["a","bc"]` and `["ab","c"]` distinct. Returns
/// 16 hex chars, the shape `mint_key` produces, so it is a drop-in for `key`.
/// `mint_key` stays correct for the only case with no stable referent: an
/// unnamed, originless squad.
pub fn origin_key(origins: &[String]) -> String {
    let mut set: Vec<&str> = origins.iter().map(|s| s.as_str()).collect();
    set.sort_unstable();
    set.dedup();
    let mut joined = String::new();
    for (i, o) in set.iter().enumerate() {
        if i > 0 {
            joined.push('\u{001f}');
        }
        joined.push_str(o);
    }
    format!("{:016x}", fnv1a(joined.as_bytes()))
}

/// One persisted squad. Named workspaces key by `name`; an unnamed squad (empty
/// `name` - a home squad or a project lane) keys by its durable `key`.
/// Deliberately NOT `Eq`: `tab_trees` carries f32 split weights (x-caef).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSquad {
    pub name: String,
    /// The durable identity of an UNNAMED squad (empty for a named one). Keeps
    /// two same-origin unnamed squads distinct in the store. `#[serde(default)]`
    /// keeps a pre-key store readable (absent -> empty), no STORE_VERSION bump.
    #[serde(default)]
    pub key: String,
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
    /// (x-caef) Every tab's full topology, in squad tab order. The successor
    /// lane to `tab_specs` for shape; restore rebuilds from these when present
    /// and falls back to the template/member lanes when absent (a pre-x-caef
    /// store loads them as `[]`). `#[serde(default)]`, same no-bump rule.
    #[serde(default)]
    pub tab_trees: Vec<StoredTabTree>,
    /// (x-caef) The squad's active tab INDEX at capture, clamped at restore.
    /// `#[serde(default)]`, same no-bump rule as `tab_trees`.
    #[serde(default)]
    pub active_tab: Option<usize>,
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

/// One persisted tab's FULL topology (x-caef): an arbitrary weighted tree
/// (`layout get`'s shape, the struct `LayoutTreeSpec` graft already uses),
/// not one of `LayoutSpec`'s five named templates. Capture is uniform across
/// every tab of a persisted squad, so a hand split survives a restart exactly
/// like a template tab does. Tabs have no id that outlives the server, so
/// identity is POSITION in the squad's `tab_trees` vec, captured atomically
/// with the whole squad; `tab_name` is the chosen name when there is one.
/// Slot names are decided at CAPTURE (an fno pane names its slot its attach
/// id and binds `Fno`; anything else is `p<ordinal>` binding `Shell`) so two
/// snapshots of one session agree on which pane is which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredTabTree {
    #[serde(default)]
    pub tab_name: Option<String>,
    pub tree: crate::proto::LayoutTreeSpec,
    pub slots: Vec<crate::proto::LayoutSlot>,
    /// The slot name that held keyboard focus at capture.
    #[serde(default)]
    pub focus: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
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
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Loaded {
    pub squads: Vec<StoredSquad>,
    /// (x-7561) The tracked external-row lifecycle tombstones, `attach_id`
    /// validated exactly like squad members (a malformed id never reaches an
    /// argv). Empty when the store has none.
    pub external_lifecycle: Vec<ExternalLifecycle>,
    pub notice: Option<String>,
}

/// The store file: a sibling of the registry under `FNO_AGENTS_HOME`, else
/// the mux's resolved state root (`squads.json`), so a pinned `FNO_CONFIG`
/// isolates the squad store with the sockets and view prefs - a demo server
/// must not read or prune the operator's real squads. Machine-global in the
/// unpinned case because a squad spans repos.
pub fn squads_path() -> PathBuf {
    #[cfg(test)]
    return TEST_PATH.with(|c| c.borrow().path());
    #[cfg(not(test))]
    if let Some(v) = std::env::var_os("FNO_AGENTS_HOME") {
        return PathBuf::from(v).join("squads.json");
    }
    #[cfg(not(test))]
    return crate::proto::mux_sidecar_root().join("squads.json");
}

/// A jobId is exactly 8 ascii-hex digits (the `claude attach` gate). File
/// content is untrusted input, so a malformed id must never reach an argv
/// (epic Boundaries; AC2-ERR): it is dropped at load, before restore can spawn.
pub fn valid_attach_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A worker name is a registry name, not a path or a shell token (x-5f7f).
/// Same argv-safety posture as [`valid_attach_id`]: file content is untrusted
/// and a worker name reaches a resume spawn keyed by name, so anything outside
/// the slug charset (`fno agents spawn` itself refuses other shapes) is
/// dropped at load. Non-empty, at most 64 chars, ascii `[A-Za-z0-9._-]` only -
/// no separator, no whitespace, no metacharacter.
pub fn valid_worker_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Load the store for restore. A missing/empty file is a fresh store (no
/// notice). An unreadable one also reads as empty, but says so. A corrupt file
/// or unknown version is renamed aside (`squads.json.corrupt-<secs>`) and read
/// as empty (AC1-ERR: never refuse to start). Members with a malformed
/// `attach_id` are dropped with a notice (AC2-ERR).
pub fn load() -> Loaded {
    let path = squads_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        // A missing file at the state-root location falls back to the
        // pre-state-root one (no copy), gated on no pinned FNO_CONFIG for the
        // same reason as view_store: an upgrading user keeps their squads, a
        // demo env inherits nothing from the operator's root.
        Err(e) if e.kind() == io::ErrorKind::NotFound => match legacy_read() {
            Ok(s) => return loaded_from_raw(&path, s),
            Err(_) => return Loaded::default(),
        },
        // Unreadable is NOT missing. Collapsing the two made a permission
        // error or non-UTF-8 content render as an empty store, so prune
        // reported nothing to do and restore brought back zero workspaces,
        // both without a word. Still never refuses (AC1-ERR); it just says so.
        Err(e) => {
            return Loaded {
                notice: Some(format!(
                    "could not read squads.json ({e}); treating as empty"
                )),
                ..Loaded::default()
            }
        }
    };
    loaded_from_raw(&path, raw)
}

/// The pre-state-root squads file, readable only under fully ambient state
/// resolution (the one gate, spelled once - see
/// `proto::legacy_fallback_allowed`). NotFound when absent or deliberately
/// overridden; every caller treats that as "no fallback".
#[cfg(not(test))]
fn legacy_read() -> io::Result<String> {
    if !crate::proto::legacy_fallback_allowed() {
        return Err(io::ErrorKind::NotFound.into());
    }
    std::fs::read_to_string(crate::proto::legacy_mux_root().with_file_name("squads.json"))
}

#[cfg(test)]
fn legacy_read() -> io::Result<String> {
    Err(io::ErrorKind::NotFound.into())
}

/// Parse the store body shared by the primary and legacy reads, so the
/// fallback path degrades exactly like the primary one.
fn loaded_from_raw(path: &std::path::Path, raw: String) -> Loaded {
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
            // The quarantine rename is the one write outside mutate_file, so the
            // same build-tree guard backstops it. cfg(test) reaches here via the
            // TEST_PATH override (the guard is compiled out), so it always
            // quarantines; a guarded-out build-tree binary leaves the corrupt
            // file in place and names the escape in the notice (reads never
            // refuse - only the rename is held back).
            #[cfg(not(test))]
            let can_quarantine = assert_writable().is_ok();
            #[cfg(test)]
            let can_quarantine = true;
            if can_quarantine {
                let _ = std::fs::rename(&path, &aside);
            }
            return Loaded {
                notice: Some(if can_quarantine {
                    format!("quarantined corrupt squads.json to {}", aside.display())
                } else {
                    "corrupt squads.json left in place; set FNO_AGENTS_HOME to quarantine from a build-tree binary".into()
                }),
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
            // (x-5f7f) A member is valid in either shape: a claude attach id,
            // or a worker registry name. Anything else (including a worker
            // name carrying a path separator or metacharacter) is dropped at
            // load before a resume can key on it.
            sq.members.retain(|m| {
                valid_attach_id(&m.attach_id) || m.worker.as_deref().is_some_and(valid_worker_name)
            });
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

/// Match a stored squad by identity: a NAMED squad (`name` non-empty) is keyed
/// by `name`; an UNNAMED squad (empty `name` - a home squad, a project lane) is
/// keyed by its durable `key`, which keeps two same-origin unnamed squads
/// distinct. An empty name + empty key has no identity and matches nothing.
fn same_squad(s: &StoredSquad, name: &str, key: &str) -> bool {
    if name.is_empty() {
        !key.is_empty() && s.name.is_empty() && s.key == key
    } else {
        s.name == name
    }
}

/// Insert-or-replace the entry with this identity (`name` if named, else the
/// durable `key`), preserving its `created_at` if it already exists (else
/// stamping now). Write-through for `NewSquad`, recruit, member close, tombstone,
/// and an unnamed home squad / project lane (operator decision: every squad
/// persists). `origins` is stored (for restore + owns_path) but is NOT identity.
pub fn upsert(
    name: &str,
    key: &str,
    origins: &[String],
    members: &[StoredMember],
) -> io::Result<()> {
    // A squad with neither a name nor a durable key has no identity across a
    // restart, so it cannot be persisted (nor found again). Skip it here, the
    // one place every write path funnels through, rather than at each caller.
    if name.is_empty() && key.is_empty() {
        return Ok(());
    }
    mutate(|squads| {
        let existing = squads.iter().find(|s| same_squad(s, name, key));
        let created_at = existing
            .map(|s| s.created_at.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(now_iso);
        // Preserve template tab specs and tab trees (owned by set_tab_specs /
        // set_tab_trees, not this path) across a membership upsert - the struct
        // is rebuilt fresh, so an un-carried field would be silently wiped
        // (x-c4d4, x-caef).
        let tab_specs = existing.map(|s| s.tab_specs.clone()).unwrap_or_default();
        let (tab_trees, active_tab) = existing
            .map(|s| (s.tab_trees.clone(), s.active_tab))
            .unwrap_or_default();
        squads.retain(|s| !same_squad(s, name, key));
        squads.push(StoredSquad {
            name: name.to_string(),
            key: key.to_string(),
            origins: origins.to_vec(),
            members: members.to_vec(),
            created_at,
            tab_specs,
            tab_trees,
            active_tab,
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
                key: String::new(), // templates are a named-squad feature
                origins: Vec::new(),
                members: Vec::new(),
                created_at: now_iso(),
                tab_specs: tab_specs.to_vec(),
                tab_trees: Vec::new(),
                active_tab: None,
            });
        }
    })
}

/// Set the full tab-topology lane for the squad with this identity (`name` if
/// named, else the durable `key` - an UNNAMED squad can hold a layout, which
/// is gate 2 of the three the old template lane dropped), preserving every
/// other field. Refuses to mint a row for an identity-less squad (the same
/// skip `upsert` applies); an identity not present inserts a minimal entry
/// only when `origins` are supplied, so the row carries what restore needs.
pub fn set_tab_trees(
    name: &str,
    key: &str,
    origins: &[String],
    tab_trees: &[StoredTabTree],
    active_tab: Option<usize>,
) -> io::Result<()> {
    if name.is_empty() && key.is_empty() {
        return Ok(()); // no identity: the same skip upsert applies
    }
    mutate(|squads| {
        if let Some(s) = squads.iter_mut().find(|s| same_squad(s, name, key)) {
            s.tab_trees = tab_trees.to_vec();
            s.active_tab = active_tab;
        } else {
            squads.push(StoredSquad {
                name: name.to_string(),
                key: key.to_string(),
                origins: origins.to_vec(),
                members: Vec::new(),
                created_at: now_iso(),
                tab_specs: Vec::new(),
                tab_trees: tab_trees.to_vec(),
                active_tab,
            });
        }
    })
}

/// Delete the entry with this identity (`name` if named, else the durable
/// `key`): a user-closed / removed workspace, or an unnamed lane whose last pane
/// closed. An identity not present is a silent no-op.
pub fn remove(name: &str, key: &str) -> io::Result<()> {
    mutate(|squads| squads.retain(|s| !same_squad(s, name, key)))
}

// --- prune: reap squads whose every origin is gone and nothing is live -----

/// Mirror of `Squad::owns_path` over a stored squad's origins: `cwd` is one of
/// `origins` or a child of one. Inlined rather than imported so the store stays
/// file-local (the crate rule: the FILE is the contract).
fn origin_owned(origins: &[String], cwd: &str) -> bool {
    origins.iter().any(|o| {
        cwd == o.as_str()
            || cwd
                .strip_prefix(o.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}

/// One squad's fate under the prune predicate (pure; re-evaluated under the
/// lock in [`prune`] against fresh fs state, so a squad a concurrent recruit
/// made live between the off-lock snapshot and the write is kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneDecision {
    /// Protected by a hard signal: a surviving origin dir, a provably-live
    /// member, or a live pane mapped to the squad.
    Keep,
    /// A member or pane whose liveness is unknown (the liveness query failed).
    /// Fail-safe: never prune on unknown - the liveness surface has lied before
    /// (corpus entry 2).
    KeepUnknown,
    /// A named squad with `--include-named` not passed. Counted separately so the
    /// receipt can name the flag (AC1-EDGE).
    SkipNamed,
    /// Unnamed (or `--include-named`), every origin gone, every member provably
    /// dead, no live pane.
    Prune,
}

/// The prune predicate (US2). `live` is the live attach-id set: `Some(set)` when
/// the off-lock liveness query succeeded, `None` when it failed (then every
/// non-tombstone member is unknown-liveness and the squad is kept). `live_cwds`
/// are the cwds of live mux panes; a pane mapped to the squad protects it.
/// `origin_exists` is injected so the matrix is unit-testable without disk; the
/// verb passes `|p| std::path::Path::new(p).exists()` and re-runs it under the
/// lock.
pub fn prune_decision(
    squad: &StoredSquad,
    include_named: bool,
    live: Option<&std::collections::HashSet<String>>,
    live_cwds: &[String],
    origin_exists: &dyn Fn(&str) -> bool,
) -> PruneDecision {
    prune_decision_at(squad, include_named, live, live_cwds, origin_exists, None)
}

/// [`prune_decision`] with the clock injected, for the empty-member grace window.
///
/// `now_epoch` is `None` when the caller has no clock to offer; then a
/// zero-member squad is KEPT rather than pruned, because without a clock a fresh
/// recruit and a finished squad are indistinguishable.
///
/// MEMBER DEADNESS OUTRANKS ORIGIN EXISTENCE, and that ordering is the fix.
/// The origin check used to run first and return `Keep` on any surviving dir.
/// Measured on this machine: all 15 squads' origins are REPO ROOTS
/// (`/code/footnote/footnote`, `/c3po`, `/.claude`), never worktrees. A repo root
/// never disappears, so that arm returned `Keep` forever and 12 of 15 finished
/// squads were immortal. A directory existing says nothing about whether anything
/// is still running in it.
pub fn prune_decision_at(
    squad: &StoredSquad,
    include_named: bool,
    live: Option<&std::collections::HashSet<String>>,
    live_cwds: &[String],
    origin_exists: &dyn Fn(&str) -> bool,
    now_epoch: Option<i64>,
) -> PruneDecision {
    if !squad.name.is_empty() && !include_named {
        return PruneDecision::SkipNamed;
    }
    // Every non-tombstone member must be provably dead. A member not in the live
    // set is dead; with the set absent (query failed) every member is unknown.
    for m in &squad.members {
        if m.tombstone {
            continue;
        }
        match live {
            None => return PruneDecision::KeepUnknown,
            Some(set) if set.contains(&m.attach_id) => return PruneDecision::Keep,
            _ => {}
        }
    }
    if !squad.members.is_empty() {
        // Member records exist and not one of them is live. That is direct
        // evidence about THIS squad, and it settles the question. No fact about
        // a directory may overturn it.
        //
        // The live-pane check used to run before this and matched any pane whose
        // cwd sits under an origin. With repo-root origins that is nearly every
        // pane on the machine, so one live session kept every finished squad in
        // the same repo. Measured: it held 9 of the 12 finished squads here,
        // including two with nine dead members each.
        return PruneDecision::Prune;
    }

    // NO MEMBER RECORDS AT ALL is the ambiguous case, and only that one.
    //
    // A tombstoned member is positive evidence that someone registered and died.
    // An EMPTY list is an absence, and a squad mid-recruit is indistinguishable
    // from one whose members are long gone. Nine of the fifteen squads measured
    // here are member-less, so this is the common case rather than an edge.
    //
    // With no members to judge by, the directory heuristics are all we have. A
    // live pane under an origin might BE this squad's unrecorded worker, a
    // vanished origin means there is nothing left to recruit into, and otherwise
    // only the clock separates a fresh squad from a finished one.
    // ONE GRACE WINDOW, BOTH ARMS. Each directory heuristic is a guess about a
    // squad too young to judge, so each expires on the same clock. The pane arm
    // used to be unconditional, and an arm that never ages beside one that ages
    // after an hour is an asymmetry inside a single predicate: measured here,
    // six member-less squads weeks old were held by panes that merely shared a
    // repo root with them. A pane in a repo is a coincidence of directory, not
    // evidence about a squad nobody has recruited into for three weeks.
    if empty_squad_past_grace(squad, now_epoch) {
        return PruneDecision::Prune;
    }
    if live_cwds
        .iter()
        .any(|cwd| origin_owned(&squad.origins, cwd))
    {
        return PruneDecision::Keep;
    }
    if squad.origins.iter().any(|o| origin_exists(o.as_str())) {
        return PruneDecision::KeepUnknown;
    }
    PruneDecision::Prune
}

/// How long a member-less squad is protected after creation. Matches the agents
/// dead-row grace so both surfaces forget a finished session on one clock.
pub const EMPTY_SQUAD_GRACE_SECS: i64 = 3600;

/// Wall-clock epoch seconds for the grace window, or `None` if unreadable.
/// `None` keeps every member-less squad, which is the safe direction.
pub fn now_epoch_secs() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// Has a member-less squad outlived the window in which it might still be
/// recruiting? Unparseable or absent `created_at` reads as NOT past grace, so an
/// unstamped squad is kept rather than destroyed on a missing field.
fn empty_squad_past_grace(squad: &StoredSquad, now_epoch: Option<i64>) -> bool {
    let Some(now) = now_epoch else {
        return false;
    };
    let Some(created) = parse_stamp_epoch(&squad.created_at) else {
        return false;
    };
    now.saturating_sub(created) > EMPTY_SQUAD_GRACE_SECS
}

/// Parse the store's cosmetic `YYYY-MM-DDThh:mm:ssZ` stamp to epoch seconds.
///
/// Hand-rolled because the stamp is a fixed shape and this crate carries no date
/// dependency. Anything that does not match returns `None`, which the caller
/// reads as "cannot age it", i.e. keep.
fn parse_stamp_epoch(stamp: &str) -> Option<i64> {
    let b = stamp.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| stamp.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Days since the epoch via the civil-from-days algorithm (Howard Hinnant's),
    // which is exact for any proleptic Gregorian date and needs no table.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + s)
}

/// One squad the prune actually removed (the receipt is built from these, under
/// the lock, never from the pre-lock candidate list - corpus entry 2).
#[derive(Debug, Clone)]
pub struct PrunedSquad {
    pub name: String,
    pub key: String,
    pub origins: Vec<String>,
    pub members: usize,
}

/// Why a removal happened, for the receipt.
///
/// A member-less squad is removed on directory heuristics and a clock, never on
/// evidence about itself, because no member was ever recorded for it. That is a
/// data defect, and a prune that deletes its traces silently erases the only
/// evidence the defect exists. So the receipt says which removals were of that
/// kind and the summary counts them.
pub fn prune_reason(sq: &PrunedSquad) -> &'static str {
    if sq.members == 0 {
        "no member ever recorded - removed on origin and age, not on its own evidence"
    } else {
        "every recorded member is dead"
    }
}

impl From<&StoredSquad> for PrunedSquad {
    fn from(s: &StoredSquad) -> Self {
        PrunedSquad {
            name: s.name.clone(),
            key: s.key.clone(),
            origins: s.origins.clone(),
            members: s.members.len(),
        }
    }
}

/// What a [`prune`] run did. Counts are classified under the lock from the same
/// decisions that drove the removals, so the receipt never disagrees with the
/// store (AC1-UI).
#[derive(Debug, Clone, Default)]
pub struct PruneOutcome {
    pub removed: Vec<PrunedSquad>,
    pub kept_unknown: usize,
    pub skipped_named: usize,
    pub kept_protected: usize,
    /// Tombstoned members reaped one-by-one out of squads that survived the
    /// whole-squad decision above. `prune_decision_at`
    /// returns `Keep`/`KeepUnknown` at the FIRST live (or unknown-liveness)
    /// member it finds, so a tombstoned member sitting beside a live one in
    /// the same squad was unreachable by any sweep before this.
    pub members_reaped: usize,
}

impl PruneOutcome {
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// True when a tombstoned member is reapable: provably dead (absent from a
/// KNOWN live set). A `None` live set (the liveness query failed) reaps
/// nothing, the same fail-safe direction `KeepUnknown` takes for whole squads.
/// `pub(crate)` so a `--dry-run` caller can preview the same count `prune`
/// would actually remove, instead of the write path being the only place
/// that knows this number.
pub(crate) fn tombstone_reapable(
    m: &StoredMember,
    live: Option<&std::collections::HashSet<String>>,
) -> bool {
    m.tombstone && live.is_some_and(|set| !set.contains(&m.attach_id))
}

/// One squad's fate under `decide` and `live`: its prune decision, and -- if
/// it survives -- how many of its members would be reaped as tombstones.
/// Shared by the real (locked) `prune` loop and `--dry-run`'s preview so the
/// two classification loops can never diverge from each other (self-review
/// finding: they used to be hand-duplicated, one over owned/drained squads,
/// one over a borrowed read-only load).
pub struct SquadFate {
    pub decision: PruneDecision,
    pub reaped_if_kept: usize,
}

pub fn classify_squad(
    sq: &StoredSquad,
    decide: &impl Fn(&StoredSquad) -> PruneDecision,
    live: Option<&std::collections::HashSet<String>>,
) -> SquadFate {
    let decision = decide(sq);
    let reaped_if_kept = if matches!(decision, PruneDecision::Prune) {
        0
    } else {
        sq.members
            .iter()
            .filter(|m| tombstone_reapable(m, live))
            .count()
    };
    SquadFate {
        decision,
        reaped_if_kept,
    }
}

/// Prune prunable squads in ONE locked mutation, returning what was actually
/// removed plus the keep/skip counts (the receipt source). `decide` is the pure
/// [`prune_decision`] re-evaluated under the store lock against fresh fs state,
/// so a squad a concurrent recruit made live between the off-lock snapshot and
/// the write is kept (AC1-FR lost-update guard). `external_lifecycle` records
/// are preserved byte-for-byte: `mutate_file` applies `decide` to squads only.
///
/// A squad that survives `decide` (Keep/KeepUnknown/SkipNamed) still has its
/// tombstoned members reaped under this SAME lock, via [`tombstone_reapable`]
/// against `live`. `decide` already ran against the squad's ORIGINAL member
/// list, so a squad that reaps to zero members here is not re-judged by the
/// empty-squad grace arm until the next `prune` call - one pass, one decision
/// per squad, no double jeopardy inside it.
pub fn prune(
    decide: impl Fn(&StoredSquad) -> PruneDecision,
    live: Option<&std::collections::HashSet<String>>,
) -> io::Result<PruneOutcome> {
    let mut out = PruneOutcome::default();
    mutate_file(|sf| {
        let mut kept = Vec::with_capacity(sf.squads.len());
        for mut sq in sf.squads.drain(..) {
            let fate = classify_squad(&sq, &decide, live);
            let counter = match fate.decision {
                PruneDecision::Prune => {
                    out.removed.push(PrunedSquad::from(&sq));
                    continue;
                }
                PruneDecision::KeepUnknown => &mut out.kept_unknown,
                PruneDecision::SkipNamed => &mut out.skipped_named,
                PruneDecision::Keep => &mut out.kept_protected,
            };
            *counter += 1;
            sq.members.retain(|m| !tombstone_reapable(m, live));
            out.members_reaped += fate.reaped_if_kept;
            kept.push(sq);
        }
        sf.squads = kept;
    })?;
    Ok(out)
}

/// Heal duplicate rows that accumulated under the old random-mint identity
/// (x-e447, unnamed; legacy shared mint keys, named). Migrate every unnamed
/// squad with origins onto its derived
/// [`origin_key`] (Locked Decision 1: the durable key IS a function of the
/// origin set), then collapse rows that now share an identity: among same-key
/// rows keep the one with the most members (tiebreak newest `created_at`),
/// MERGE every dropped row's members and tab specs into it, and drop the rest.
/// An originless unnamed squad keeps its random key and never collapses on it
/// (Locked Decision 2: no stable identity).
///
/// A second pass heals named duplicates: two named rows sharing one legacy
/// random `key` (a pre-origin-identity `mint_key`, unique per squad, so a
/// shared one proves common descent) collapse to one row named after the
/// NEWEST `created_at` entry - the name the operator chose most recently.
/// Deliberately different from the unnamed pass's most-members rule: the name
/// is the operator's most recent choice, not a popularity contest; do not
/// "fix" it back. A shared key that equals `origin_key` of the rows' own
/// origins proves nothing (two same-origin squads each derived it) and both
/// rows survive.
///
/// One locked mutation, prune-shaped; a write error surfaces to the caller
/// rather than healing on one machine and silently staying broken on another.
pub fn collapse_duplicate_squads() -> io::Result<usize> {
    let mut dropped = 0usize;
    mutate_file(|sf| {
        for sq in sf.squads.iter_mut() {
            if sq.name.is_empty() && !sq.origins.is_empty() {
                sq.key = origin_key(&sq.origins);
            }
        }
        let mut unnamed: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        let mut named: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, sq) in sf.squads.iter().enumerate() {
            if sq.key.is_empty() {
                continue;
            }
            if sq.name.is_empty() {
                unnamed.entry(sq.key.clone()).or_default().push(i);
            } else if sq.key != origin_key(&sq.origins) {
                named.entry(sq.key.clone()).or_default().push(i);
            }
        }
        let mut drop_idx = collapse_groups(&mut sf.squads, &unnamed, false);
        drop_idx.extend(collapse_groups(&mut sf.squads, &named, true));
        if drop_idx.is_empty() {
            return;
        }
        drop_idx.sort_unstable();
        drop_idx.dedup();
        let mut kept = Vec::with_capacity(sf.squads.len().saturating_sub(drop_idx.len()));
        for (i, sq) in sf.squads.drain(..).enumerate() {
            if drop_idx.binary_search(&i).is_ok() {
                dropped += 1;
            } else {
                kept.push(sq);
            }
        }
        sf.squads = kept;
    })?;
    Ok(dropped)
}

/// Merge each same-key group of size >= 2 into one survivor, returning the
/// dropped indices. `newest_wins` picks the survivor by newest `created_at`
/// (the named pass - keep the most recently chosen name); else most members
/// wins (the unnamed pass). Members and tab specs merge and dedup identically
/// in both: by `attach_id` with a live member beating a tombstone, and by
/// `tab_name`.
fn collapse_groups(
    squads: &mut [StoredSquad],
    groups: &std::collections::BTreeMap<String, Vec<usize>>,
    newest_wins: bool,
) -> Vec<usize> {
    let mut drop_idx = Vec::new();
    for idxs in groups.values() {
        if idxs.len() < 2 {
            continue;
        }
        let mut ranked: Vec<usize> = idxs.clone();
        ranked.sort_by(|&a, &b| {
            if newest_wins {
                squads[b].created_at.cmp(&squads[a].created_at)
            } else {
                squads[b]
                    .members
                    .len()
                    .cmp(&squads[a].members.len())
                    .then_with(|| squads[b].created_at.cmp(&squads[a].created_at))
            }
        });
        let surv = ranked[0];
        for &i in &ranked[1..] {
            let extra_members = std::mem::take(&mut squads[i].members);
            let extra_specs = std::mem::take(&mut squads[i].tab_specs);
            let extra_trees = std::mem::take(&mut squads[i].tab_trees);
            squads[surv].members.extend(extra_members);
            squads[surv].tab_specs.extend(extra_specs);
            squads[surv].tab_trees.extend(extra_trees);
            drop_idx.push(i);
        }
        // Dedup merged members by attach_id; a live member (tombstone=false)
        // wins over a tombstone of the same id.
        squads[surv].members.sort_by(|a, b| {
            a.attach_id
                .cmp(&b.attach_id)
                .then(a.tombstone.cmp(&b.tombstone))
        });
        squads[surv]
            .members
            .dedup_by(|a, b| a.attach_id == b.attach_id);
        squads[surv]
            .tab_specs
            .sort_by(|a, b| a.tab_name.cmp(&b.tab_name));
        squads[surv]
            .tab_specs
            .dedup_by(|a, b| a.tab_name == b.tab_name);
        // Trees have no durable per-tab key (identity is position, x-caef), so
        // a merge dedups only exact duplicates - a would-be reshape by the heal
        // is wrong, and any real divergence survives until the next topology
        // mutation rewrites the survivor's whole vec.
        squads[surv].tab_trees.dedup_by(|a, b| a == b);
    }
    drop_idx
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
        let (tab_trees, active_tab) = existing
            .map(|s| (s.tab_trees.clone(), s.active_tab))
            .unwrap_or_default();
        squads.retain(|s| s.name != old && s.name != new);
        squads.push(StoredSquad {
            name: new.to_string(),
            key: String::new(), // rename is a named->named op
            origins: origins.to_vec(),
            members: members.to_vec(),
            created_at,
            tab_specs,
            tab_trees,
            active_tab,
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

/// Walk `exe`'s ancestors for a directory named `target` carrying a cargo
/// build-tree marker (`.rustc_info.json` or `CACHEDIR.TAG`). Every exec'd test
/// binary lives under `target/{debug,release,...}`; no installed binary
/// (`~/.cargo/bin`, homebrew, a deployed `fno`) does. Pure over the path so the
/// marker logic is unit-testable; [`assert_writable`] feeds it `current_exe()`.
/// A bare directory named `target` without a marker is NOT a build tree (the
/// marker is what proves cargo owns it), so a coincidentally-named dir never
/// trips the guard.
fn build_tree_target_dir(exe: &std::path::Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|d| d.file_name().is_some_and(|n| n == "target"))
        .filter(|d| d.join(".rustc_info.json").exists() || d.join("CACHEDIR.TAG").exists())
        .map(|d| d.to_path_buf())
}

/// Refuse a squad-store write from a build-tree binary unless `FNO_AGENTS_HOME`
/// is set (the sole escape hatch: tests point it at a temp dir, dogfooding at
/// `$HOME/.fno`). The call site in [`mutate_file`] is `#[cfg(not(test))]`: an
/// in-process unit test already isolates via [`set_test_path`], and the guard
/// would otherwise fire on every one of them (they all run under `target/`).
/// It covers the OTHER arm - the exec'd binary and the library as linked into
/// integration tests - where the thread-local override does not exist and
/// [`squads_path`] resolves the real `$HOME/.fno`. Forgetting the env var then
/// refuses loudly instead of silently polluting the user's store.
#[cfg(not(test))]
fn assert_writable() -> io::Result<()> {
    if std::env::var_os("FNO_AGENTS_HOME").is_some() {
        return Ok(());
    }
    if build_tree_target_dir(&std::env::current_exe()?).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to write ~/.fno/squads.json from a build-tree binary; \
             set FNO_AGENTS_HOME (tests: a temp dir; dogfooding: $HOME/.fno)",
        ));
    }
    Ok(())
}

/// The locked read-modify-write core over the WHOLE [`StoreFile`]: acquire the
/// exclusive non-blocking lock, re-read the current file (empty/absent = fresh,
/// any other read/parse error FAILS LOUD rather than clobber unread content),
/// apply `f` to both collections at once, pin the version, and atomically
/// rename a tmp over the target. `mutate` / `mutate_lifecycle` are thin views
/// onto it, so every mutation preserves both collections.
fn mutate_file(f: impl FnOnce(&mut StoreFile)) -> io::Result<()> {
    #[cfg(not(test))]
    assert_writable()?;
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

    // The seed read mirrors load()'s resolution, legacy fallback included:
    // seeding from NotFound alone would collapse the store to just this one
    // mutation on the first write after the root moves, silently dropping
    // every persisted squad while prune reports the ones it just erased.
    let seed = std::fs::read_to_string(&path).or_else(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            legacy_read()
        } else {
            Err(e)
        }
    });
    let mut file = match seed {
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
            cwd: None,
            worker: None,
        }
    }

    #[test]
    fn default_test_store_never_targets_the_user_home() {
        super::clear_test_path();
        let path = squads_path();
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert!(
            !path.starts_with(&home),
            "an unscoped unit test must not persist mux squads under HOME: {}",
            path.display()
        );
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
        assert!(
            loaded.squads[0].tab_specs.is_empty(),
            "tab_specs defaults to empty"
        );
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
        upsert("w", "", &["/r".into()], &[m("c19cd2c3")]).unwrap();
        set_tab_specs("w", std::slice::from_ref(&spec)).unwrap();
        assert_eq!(load().squads[0].tab_specs, vec![spec.clone()]);
        // A later membership upsert must NOT wipe the template specs (they are
        // owned by set_tab_specs, and upsert rebuilds the struct fresh).
        upsert("w", "", &["/r".into()], &[m("c19cd2c3"), m("deadbeef")]).unwrap();
        let after = load();
        assert_eq!(after.squads[0].members.len(), 2, "membership updated");
        assert_eq!(
            after.squads[0].tab_specs,
            vec![spec],
            "tab_specs preserved across upsert"
        );
    }

    #[test]
    fn tab_trees_persist_by_key_for_an_unnamed_squad_and_survive_upsert() {
        // x-caef gate 2: topology keys by squad IDENTITY (name or key), so an
        // unnamed squad - the operator's `commanders` case, a named tab in an
        // unnamed squad - holds a layout. A membership upsert must not wipe it.
        use crate::proto::{LayoutBinding, LayoutSlot, LayoutTreeChild, LayoutTreeSpec};
        use crate::tree::Axis;
        let _s = Scratch::new("tab-trees");
        let tree = StoredTabTree {
            tab_name: Some("commanders".into()),
            tree: LayoutTreeSpec::Split {
                axis: Axis::Horizontal,
                children: vec![
                    LayoutTreeChild {
                        weight: 0.407,
                        tree: LayoutTreeSpec::Slot("aaaaaaaa".into()),
                    },
                    LayoutTreeChild {
                        weight: 0.593,
                        tree: LayoutTreeSpec::Split {
                            axis: Axis::Vertical,
                            children: vec![
                                LayoutTreeChild {
                                    weight: 0.5,
                                    tree: LayoutTreeSpec::Slot("p1".into()),
                                },
                                LayoutTreeChild {
                                    weight: 0.5,
                                    tree: LayoutTreeSpec::Slot("p2".into()),
                                },
                            ],
                        },
                    },
                ],
            },
            slots: vec![
                LayoutSlot {
                    name: "aaaaaaaa".into(),
                    binding: LayoutBinding::Fno("aaaaaaaa".into()),
                },
                LayoutSlot {
                    name: "p1".into(),
                    binding: LayoutBinding::Shell,
                },
                LayoutSlot {
                    name: "p2".into(),
                    binding: LayoutBinding::Shell,
                },
            ],
            focus: Some("p1".into()),
        };
        // An UNNAMED squad, keyed by its durable key with no name.
        set_tab_trees(
            "",
            "k1",
            &["/repo".into()],
            std::slice::from_ref(&tree),
            Some(0),
        )
        .unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads.len(),
            1,
            "minimal row minted for the keyed lane"
        );
        assert_eq!(loaded.squads[0].tab_trees, vec![tree.clone()]);
        assert_eq!(loaded.squads[0].active_tab, Some(0));
        // A membership upsert (same identity) preserves the tree lane.
        upsert("", "k1", &["/repo".into()], &[m("aaaaaaaa")]).unwrap();
        let after = load();
        assert_eq!(after.squads.len(), 1);
        assert_eq!(
            after.squads[0].tab_trees,
            vec![tree.clone()],
            "trees survive upsert"
        );
        assert_eq!(after.squads[0].members.len(), 1);
        // An identity-less squad is skipped, exactly like upsert.
        set_tab_trees("", "", &[], std::slice::from_ref(&tree), None).unwrap();
        assert_eq!(load().squads.len(), 1, "no row minted without identity");
    }

    #[test]
    fn pre_xcaef_store_loads_without_tab_trees_field() {
        // Wire tolerance: a store written before x-caef has neither key. It
        // must load unquarantined (STORE_VERSION unchanged), trees empty, and
        // restore takes the legacy member/template lanes.
        let s = Scratch::new("no-tab-trees");
        let raw = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[],"created_at":"2026-08-11T00:00:00Z","tab_specs":[]}]}"#;
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert!(loaded.squads[0].tab_trees.is_empty());
        assert_eq!(loaded.squads[0].active_tab, None);
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn pre_xcaef_member_loads_without_cwd_field() {
        // Wire tolerance for StoredMember.cwd, same rule as tab_trees above: a
        // member row written before x-caef has no "cwd" key and must load
        // unquarantined with cwd defaulting to None.
        let s = Scratch::new("no-member-cwd");
        let raw = r#"{"version":1,"squads":[{"name":"w","origins":[],"members":[{"attach_id":"c19cd2c3","tombstone":false}],"created_at":"2026-08-11T00:00:00Z","tab_specs":[]}]}"#;
        std::fs::write(s.file(), raw).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "not quarantined");
        assert_eq!(loaded.squads[0].members[0].cwd, None);
    }

    #[test]
    fn upsert_then_load_roundtrips_and_preserves_created_at() {
        let _s = Scratch::new("roundtrip");
        upsert("harden", "", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        let first = load();
        assert_eq!(first.squads.len(), 1);
        let created = first.squads[0].created_at.clone();
        assert!(created.ends_with('Z') && created.len() == 20, "{created}");
        // A second upsert (new members) keeps the original created_at.
        upsert(
            "harden",
            "",
            &["/repo".into()],
            &[m("c19cd2c3"), m("deadbeef")],
        )
        .unwrap();
        let second = load();
        assert_eq!(second.squads.len(), 1, "upsert replaces, never dupes");
        assert_eq!(second.squads[0].members.len(), 2);
        assert_eq!(second.squads[0].created_at, created, "created_at preserved");
    }

    #[test]
    fn valid_worker_name_gate() {
        // x-5f7f: the worker field is a registry name that keys a resume, so
        // the same argv-safety posture as valid_attach_id - a hostile value
        // must never survive load. Registry names are slugs; anything else
        // (separator, whitespace, metachar, overlong, non-ascii) is refused.
        assert!(valid_worker_name("probe-x5f7f"));
        assert!(valid_worker_name("t-xf730-sonnet"));
        assert!(valid_worker_name("a.b_c"));
        assert!(!valid_worker_name(""));
        assert!(!valid_worker_name("a/b"), "path separator");
        assert!(!valid_worker_name("a b"), "whitespace");
        assert!(!valid_worker_name("a;rm"), "shell metacharacter");
        assert!(!valid_worker_name("$(x)"), "command substitution");
        assert!(!valid_worker_name(&"x".repeat(65)), "overlong");
        assert!(!valid_worker_name("héllo"), "non-ascii");
    }

    #[test]
    fn worker_member_roundtrips_without_a_jobid() {
        // x-5f7f: a worker member carries a registry NAME and an EMPTY
        // attach_id (a codex/agy pane has no claude jobId). It must round-trip
        // through the store and survive the load gate, which previously
        // dropped every member whose attach_id was not 8 hex digits - the
        // measured reason a widened field alone would have shipped nothing.
        let _s = Scratch::new("worker-roundtrip");
        let worker = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tab_name: Some("lane".into()),
            cwd: Some("/repo/wt".into()),
            worker: Some("probe-x5f7f".into()),
        };
        upsert("work", "", &["/repo".into()], &[worker, m("c19cd2c3")]).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1, "no quarantine");
        assert!(loaded.notice.is_none(), "{:?}", loaded.notice);
        let members = &loaded.squads[0].members;
        assert_eq!(members.len(), 2, "worker member survives the load gate");
        let w = members
            .iter()
            .find(|m| m.worker.is_some())
            .expect("worker member present");
        assert_eq!(w.worker.as_deref(), Some("probe-x5f7f"));
        assert_eq!(w.attach_id, "", "no claude jobId on a worker member");
        assert_eq!(w.tab_name.as_deref(), Some("lane"), "tab name round-trips");
        assert_eq!(w.cwd.as_deref(), Some("/repo/wt"));
    }

    #[test]
    fn hostile_worker_name_is_dropped_at_load() {
        // The load gate's argv-safety half: a worker name carrying a path
        // separator or a metacharacter never reaches a resume, exactly like a
        // malformed attach_id never reaches `claude attach`.
        let _s = Scratch::new("worker-hostile");
        let hostile = StoredMember {
            attach_id: String::new(),
            tombstone: false,
            tab_name: None,
            cwd: None,
            worker: Some("a;rm -rf".into()),
        };
        upsert("work", "", &["/repo".into()], &[hostile, m("c19cd2c3")]).unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads[0].members.len(),
            1,
            "hostile worker member dropped at load"
        );
        assert_eq!(
            loaded.squads[0].members[0].attach_id, "c19cd2c3",
            "the healthy member is untouched"
        );
        assert!(loaded.notice.is_some(), "the drop is named, never silent");
    }

    #[test]
    fn four_squad_store_on_disk_loads_whole() {
        // x-5f7f: the exact store shape measured on the operator's disk -
        // four squads, three holding ZERO members (the empty-squads defect:
        // worker panes never entered the membership funnel) and one holding
        // six claude attach members. The widened member must not quarantine or
        // notice on any of them; the fourth squad exercises a named row.
        let s = Scratch::new("four-squads");
        let members = [
            "119e3c52", "cbd219bd", "f5996a81", "3d9938aa", "a1b2c3d4", "e5f60718",
        ]
        .iter()
        .map(|id| {
            serde_json::json!({
                "attach_id": id,
                "tombstone": false,
                "tab_name": null,
                "cwd": "/wt/a"
            })
        })
        .collect::<Vec<_>>();
        let raw = serde_json::json!({
            "version": 1,
            "squads": [
                {"name": "", "key": "1111111111111111", "origins": ["/repo"],
                 "members": members, "created_at": "2026-08-21T00:00:00Z"},
                {"name": "", "key": "2222222222222222", "origins": ["/gone"],
                 "members": [], "created_at": "2026-08-21T00:00:00Z"},
                {"name": "", "key": "3333333333333333", "origins": ["/gone2"],
                 "members": [], "created_at": "2026-08-21T00:00:00Z"},
                {"name": "x-f3d0", "key": "", "origins": ["/repo"],
                 "members": [], "created_at": "2026-08-21T00:00:00Z"}
            ]
        });
        std::fs::write(s.file(), serde_json::to_string(&raw).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 4, "all four load");
        assert!(loaded.notice.is_none(), "{:?}", loaded.notice);
        assert_eq!(
            loaded.squads[0].members.len(),
            6,
            "the six claude members survive"
        );
        assert!(loaded.squads.iter().any(|sq| sq.name == "x-f3d0"));
    }

    #[test]
    fn tab_name_roundtrips_and_absent_field_loads_none() {
        // x-0f9d US4: a member's tab_name persists and reloads; a pre-x-0f9d
        // store written without the field is wire-tolerant (loads as None ->
        // the tab restores unnamed), so STORE_VERSION stays 1.
        let s = Scratch::new("tabname");
        let mut named = m("c19cd2c3");
        named.tab_name = Some("reviews".into());
        upsert("work", "", &["/repo".into()], &[named]).unwrap();
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
                key: String::new(),
                origins: vec![],
                members: vec![
                    m("c19cd2c3"),  // good
                    m("; rm -rf"),  // shell metachar
                    m("deadbeef9"), // 9 chars
                    m("GHIJKLmn"),  // non-hex
                ],
                created_at: "2026-07-11T00:00:00Z".into(),
                tab_specs: vec![],
                tab_trees: Vec::new(),
                active_tab: None,
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
        upsert("a", "", &[], &[m("11111111")]).unwrap();
        upsert("b", "", &[], &[m("22222222")]).unwrap();
        rename("a", "aa", &["/x".into()], &[m("11111111")]).unwrap();
        remove("b", "").unwrap();
        let loaded = load();
        let names: Vec<_> = loaded.squads.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aa"], "a renamed, b removed");
        assert_eq!(loaded.squads[0].origins, vec!["/x".to_string()]);
    }

    #[test]
    fn unnamed_squad_persists_keyed_by_durable_key() {
        // Operator decision: every squad remains across restart, not only named
        // workspaces. An unnamed squad (empty name - the home squad, a lane) is
        // keyed by its durable KEY, not its origins - so two same-origin unnamed
        // squads stay distinct (the codex P1: two cleared names, or two sessions'
        // home for one repo). Same key -> replace in place; distinct keys with the
        // SAME origins -> two entries; a keyless unnamed squad is not persisted.
        let _s = Scratch::new("unnamed-key");
        upsert("", "k1", &["/repo".into()], &[m("aaaaaaaa")]).unwrap();
        upsert("", "k2", &["/repo".into()], &[m("bbbbbbbb")]).unwrap();
        // Same key -> replace, not duplicate.
        upsert("", "k1", &["/repo".into()], &[m("aaaaaaaa"), m("cccccccc")]).unwrap();
        // No name and no key -> no identity -> skipped silently.
        upsert("", "", &["/repo".into()], &[m("dddddddd")]).unwrap();
        let loaded = load();
        assert_eq!(
            loaded.squads.len(),
            2,
            "two same-origin unnamed squads stay distinct by key; the keyless one is skipped"
        );
        let k1 = loaded
            .squads
            .iter()
            .find(|s| s.key == "k1")
            .expect("lane k1 persisted by key");
        assert!(k1.name.is_empty(), "restored unnamed");
        assert_eq!(k1.members.len(), 2, "same-key upsert replaced in place");
        // Remove by key drops exactly that lane, leaving its same-origin sibling.
        remove("", "k1").unwrap();
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(loaded.squads[0].key, "k2");
        assert_eq!(loaded.squads[0].origins, vec!["/repo".to_string()]);
    }

    #[test]
    fn origin_key_is_stable_order_independent_and_distinct_per_set() {
        // x-e447: an unnamed squad's durable key is a pure function of its origin
        // SET, so one repo's home squad derives one key across restarts. Order-
        // independent and duplicate-insensitive (sorted + deduped), distinct per
        // distinct set, and the separator keeps adjacent-path sets apart.
        assert_eq!(
            origin_key(&["/a".into(), "/b".into()]),
            origin_key(&["/b".into(), "/a".into()]),
            "order does not matter"
        );
        assert_eq!(
            origin_key(&["/a".into(), "/a".into()]),
            origin_key(&["/a".into()]),
            "duplicate origins collapse"
        );
        assert_eq!(
            origin_key(&["/a".into()]).len(),
            16,
            "16 hex chars, mint_key shape"
        );
        assert_ne!(
            origin_key(&["/ab".into()]),
            origin_key(&["/a".into(), "b".into()]),
            "separator keeps adjacent sets distinct"
        );
        assert_ne!(
            origin_key(&["/repo".into()]),
            origin_key(&["/other".into()]),
            "distinct origins derive distinct keys"
        );
    }

    #[test]
    fn collapse_duplicate_squads_merges_same_origin_into_one() {
        // x-e447 AC-HP2: the backlog rows carry distinct random keys (the old
        // mint), so a key-based upsert cannot heal them. collapse groups by
        // origin, rekeys onto origin_key, keeps the membered row, merges every
        // dropped row's members, and leaves exactly one row per origin. A named
        // squad sharing the origin is NOT absorbed (it keys by name).
        let _s = Scratch::new("collapse-same-origin");
        let repo = "/repo/backlog";
        let one = origin_key(&[repo.into()]);
        upsert("", "dead0001", &[repo.into()], &[]).unwrap();
        upsert("", "dead0002", &[repo.into()], &[m("aaaaaaaa")]).unwrap();
        upsert("", "dead0003", &[repo.into()], &[m("bbbbbbbb")]).unwrap();
        // A different origin and a named squad stay separate.
        upsert("", "other1", &["/other".into()], &[]).unwrap();
        upsert("named", "", &[repo.into()], &[m("cccccccc")]).unwrap();

        let dropped = collapse_duplicate_squads().unwrap();
        assert_eq!(dropped, 2, "collapsed the two extra same-origin rows");

        let loaded = load();
        let same_origin: Vec<_> = loaded
            .squads
            .iter()
            .filter(|s| s.name.is_empty() && s.origins == vec![repo.to_string()])
            .collect();
        assert_eq!(same_origin.len(), 1, "one row for the origin");
        assert_eq!(
            same_origin[0].key, one,
            "migrated onto the derived origin key"
        );
        assert_eq!(
            same_origin[0].members.len(),
            2,
            "members merged into the survivor"
        );
        assert_eq!(
            loaded.squads.len(),
            3,
            "named + other-origin + the one collapsed row"
        );
        assert!(
            loaded.squads.iter().any(|s| s.name == "named"),
            "named squad untouched"
        );
    }

    #[test]
    fn collapse_duplicate_squads_is_idempotent() {
        // x-e447: collapse runs every restore; a second pass must be a no-op
        // (drops nothing) once the store has converged.
        let _s = Scratch::new("collapse-idempotent");
        upsert("", "k1", &["/r".into()], &[m("aaaaaaaa")]).unwrap();
        upsert("", "k2", &["/r".into()], &[]).unwrap();
        assert_eq!(collapse_duplicate_squads().unwrap(), 1);
        assert_eq!(collapse_duplicate_squads().unwrap(), 0, "second pass no-op");
        let loaded = load();
        assert_eq!(loaded.squads.len(), 1);
        assert_eq!(loaded.squads[0].key, origin_key(&["/r".into()]));
    }

    #[test]
    fn collapse_duplicate_squads_surfaces_a_write_error() {
        // x-e447 AC-ERR1: a collapse write error is not swallowed. A corrupt
        // store makes the locked read fail loud, and collapse returns Err so the
        // caller degrades restore instead of healing silently on one machine.
        let s = Scratch::new("collapse-err");
        std::fs::write(s.file(), "{garbage not json").unwrap();
        assert!(collapse_duplicate_squads().is_err());
    }

    #[test]
    fn collapse_duplicate_named_merges_a_legacy_shared_key_newest_name_wins() {
        // AC5-HP: two named rows sharing one legacy random mint key are one
        // workspace wearing two names (a mint is unique per squad). They
        // collapse to the NEWEST created_at row's name with the union of both
        // member lists. `prune --include-named` cannot reach this pair (the
        // origin still exists), so this collapse is the only heal.
        let s = Scratch::new("collapse-named-legacy");
        let legacy = "25a5abd2af1696a0";
        let origins = vec!["/repo".into()];
        assert_ne!(
            legacy,
            origin_key(&origins),
            "precondition: a legacy mint, not a derived origin key"
        );
        let file = StoreFile {
            version: STORE_VERSION,
            squads: vec![
                StoredSquad {
                    name: "f[no]".into(),
                    key: legacy.into(),
                    origins: origins.clone(),
                    members: vec![m("aaaaaaaa"), m("bbbbbbbb")],
                    created_at: "2026-07-23T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: "fno".into(),
                    key: legacy.into(),
                    origins,
                    members: vec![m("cccccccc")],
                    created_at: "2026-07-26T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            external_lifecycle: vec![],
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();

        let dropped = collapse_duplicate_squads().unwrap();
        assert_eq!(dropped, 1, "the older duplicate row is gone");

        let loaded = load();
        let rows: Vec<_> = loaded
            .squads
            .iter()
            .filter(|r| r.name == "fno" || r.name == "f[no]")
            .collect();
        assert_eq!(rows.len(), 1, "one row survives");
        assert_eq!(rows[0].name, "fno", "the newest created_at row's name wins");
        for id in ["aaaaaaaa", "bbbbbbbb", "cccccccc"] {
            assert!(
                rows[0].members.iter().any(|x| x.attach_id == id),
                "member {id} merged into the survivor"
            );
        }
    }

    #[test]
    fn collapse_duplicate_named_spares_same_origin_derived_keys() {
        // AC6-EDGE: two named rows sharing a key that EQUALS origin_key of
        // their own origins are two same-origin squads that each derived it -
        // common key proves nothing, and both rows must survive the heal.
        let s = Scratch::new("collapse-named-derived");
        let key = origin_key(&["/repo".into()]);
        let file = StoreFile {
            version: STORE_VERSION,
            squads: vec![
                StoredSquad {
                    name: "one".into(),
                    key: key.clone(),
                    origins: vec!["/repo".into()],
                    members: vec![m("aaaaaaaa")],
                    created_at: "2026-07-23T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
                StoredSquad {
                    name: "two".into(),
                    key,
                    origins: vec!["/repo".into()],
                    members: vec![m("bbbbbbbb")],
                    created_at: "2026-07-26T00:00:00Z".into(),
                    tab_specs: vec![],
                    tab_trees: Vec::new(),
                    active_tab: None,
                },
            ],
            external_lifecycle: vec![],
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();

        let dropped = collapse_duplicate_squads().unwrap();
        assert_eq!(dropped, 0, "derived shared keys are not duplicates");
        let loaded = load();
        assert!(loaded.squads.iter().any(|r| r.name == "one"));
        assert!(loaded.squads.iter().any(|r| r.name == "two"));
    }

    #[test]
    fn write_onto_a_corrupt_file_fails_loud_and_never_clobbers() {
        // gemini review: the write path must NOT clobber unreadable content. A
        // corrupt existing file makes upsert fail (Err) rather than overwrite it
        // with just this delta - the load path owns quarantine, not the writer.
        let s = Scratch::new("write-corrupt");
        std::fs::write(s.file(), "{garbage not json").unwrap();
        let before = std::fs::read_to_string(s.file()).unwrap();
        let res = upsert("w", "", &[], &[m("c19cd2c3")]);
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

    #[test]
    fn build_tree_marker_detection_is_pure_over_path() {
        // The guard's detector: an ancestor `target` with a cargo marker is a
        // build tree; a coincidental `target` dir without one is not, and a
        // binary with no `target` ancestor (an install) never is. Pure over the
        // path so it needs no env and no real build tree.
        let tmp = std::env::temp_dir().join(format!("fno-guard-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let target = tmp.join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        let exe = target.join("debug").join("fno");

        // No marker -> a bare `target` dir is NOT a build tree.
        assert_eq!(build_tree_target_dir(&exe), None);

        // `.rustc_info.json` is the cargo marker written into target/.
        std::fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        assert_eq!(build_tree_target_dir(&exe), Some(target.clone()));

        // `CACHEDIR.TAG` alone also qualifies (llvm-cov / build tooling).
        std::fs::remove_file(target.join(".rustc_info.json")).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), "x").unwrap();
        assert_eq!(build_tree_target_dir(&exe), Some(target));

        // An installed binary (no `target` ancestor) is never a build tree.
        std::fs::create_dir_all(tmp.join("bin")).unwrap();
        assert_eq!(build_tree_target_dir(&tmp.join("bin").join("fno")), None);

        let _ = std::fs::remove_dir_all(&tmp);
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
        upsert("w", "", &["/repo".into()], &[m("c19cd2c3")]).unwrap();
        assert!(matches!(
            begin_external_stop("deadbeef", "ext", "/tmp").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));
        // A SECOND squad write must not clobber the lifecycle record.
        upsert("w2", "", &[], &[m("11111111")]).unwrap();
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

    #[test]
    fn prune_predicate_matrix() {
        use std::collections::HashSet;
        let live: HashSet<String> = ["live0001".into()].into_iter().collect();
        let live_some = Some(&live);
        let no_cwds: Vec<String> = Vec::new();
        let gone = |_: &str| false; // no origin dir exists
        let exists = |p: &str| p == "/alive";

        let squad = |name: &str, key: &str, origins: &[&str], members: &[&str]| StoredSquad {
            name: name.into(),
            key: key.into(),
            origins: origins.iter().map(|s| (*s).to_string()).collect(),
            members: members.iter().copied().map(m).collect(),
            created_at: String::new(),
            tab_specs: Vec::new(),
            tab_trees: Vec::new(),
            active_tab: None,
        };

        // Named without --include-named -> SkipNamed (AC1-EDGE).
        assert_eq!(
            prune_decision(
                &squad("work", "", &["/g"], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::SkipNamed
        );
        // Named WITH --include-named, gone origin, dead member -> Prune.
        assert_eq!(
            prune_decision(
                &squad("work", "", &["/g"], &["deadbeef"]),
                true,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // Unnamed, gone, dead -> Prune.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // A surviving origin no longer outranks member deadness. This assertion
        // used to expect Keep, and that expectation WAS the defect: every squad
        // origin measured on this machine is a repo root, a directory that never
        // disappears, so the old arm kept 12 of 15 finished squads immortal. A
        // directory existing says nothing about whether anything runs in it.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/alive", "/g"], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &exists
            ),
            PruneDecision::Prune
        );
        // A live member -> Keep.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &["live0001"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Keep
        );
        // Liveness query failed (None) with a non-tombstone member -> KeepUnknown (AC3-FR).
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &["deadbeef"]),
                false,
                None,
                &no_cwds,
                &gone
            ),
            PruneDecision::KeepUnknown
        );
        // Empty members, gone -> Prune (a side-effect squad with nothing live).
        assert_eq!(
            prune_decision(
                &squad("", "k1", &["/g"], &[]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // Empty origins (no surviving origin), dead -> Prune.
        assert_eq!(
            prune_decision(
                &squad("", "k1", &[], &["deadbeef"]),
                false,
                live_some,
                &no_cwds,
                &gone
            ),
            PruneDecision::Prune
        );
        // All-tombstone members, gone -> Prune (tombstones are not live).
        {
            let mut s = squad("", "k1", &["/g"], &[]);
            s.members = vec![StoredMember {
                attach_id: "deadbeef".into(),
                tombstone: true,
                tab_name: None,
                cwd: None,
                worker: None,
            }];
            assert_eq!(
                prune_decision(&s, false, live_some, &no_cwds, &gone),
                PruneDecision::Prune
            );
        }
        // A live pane no longer overrides a squad's OWN dead members. The pane
        // is matched by "cwd sits under an origin", and with repo-root origins
        // that is nearly every pane on the machine: one live session kept 9 of
        // the 12 finished squads measured here, two of them with nine dead
        // members each. A member-less squad still gets this protection, since
        // there the pane may BE its unrecorded worker (asserted just below).
        {
            let cwds = vec!["/gone/child".to_string()];
            assert_eq!(
                prune_decision(
                    &squad("", "k1", &["/gone"], &["deadbeef"]),
                    false,
                    live_some,
                    &cwds,
                    &gone
                ),
                PruneDecision::Prune
            );
        }
        // ...but a MEMBER-LESS squad with a live pane under its origin is kept.
        {
            let cwds = vec!["/gone/child".to_string()];
            assert_eq!(
                prune_decision(
                    &squad("", "k1", &["/gone"], &[]),
                    false,
                    live_some,
                    &cwds,
                    &gone
                ),
                PruneDecision::Keep
            );
        }
    }

    // -- The empty-member grace window --------------------------------------
    //
    // Nine of the fifteen squads measured on this machine have ZERO members. A
    // squad mid-recruit and a squad whose members are long gone look identical
    // there, and only the clock separates them, so this is the case most likely
    // to destroy something a person is still using.

    /// A member-less squad stamped `created_at`, for the grace tests.
    fn empty_squad(created_at: &str) -> StoredSquad {
        StoredSquad {
            name: String::new(),
            key: "k1".into(),
            origins: vec!["/alive".into()],
            members: Vec::new(),
            created_at: created_at.into(),
            tab_specs: Vec::new(),
            tab_trees: Vec::new(),
            active_tab: None,
        }
    }

    /// 2026-08-13T12:00:00Z in epoch seconds, from an INDEPENDENT implementation
    /// (`python3 -c "datetime.fromisoformat(...).timestamp()"`), so a wrong
    /// parser cannot agree with itself. The first value written here was wrong
    /// and this assertion is what caught it.
    const T_NOON: i64 = 1_786_622_400;

    #[test]
    fn stamp_parser_matches_a_known_epoch() {
        assert_eq!(parse_stamp_epoch("2026-08-13T12:00:00Z"), Some(T_NOON));
        assert_eq!(parse_stamp_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_stamp_epoch("2000-03-01T00:00:00Z"), Some(951_868_800));
        // Leap day, the case a naive month table gets wrong.
        assert_eq!(
            parse_stamp_epoch("2024-02-29T00:00:00Z"),
            Some(1_709_164_800)
        );
    }

    #[test]
    fn unparseable_stamp_keeps_the_squad() {
        // Cannot age it -> cannot claim it is finished.
        for bad in [
            "",
            "not-a-date",
            "2026-08-13",
            "20260813T120000Z",
            "xxxx-08-13T12:00:00Z",
        ] {
            assert_eq!(parse_stamp_epoch(bad), None, "{bad:?} must not parse");
            assert_eq!(
                prune_decision_at(
                    &empty_squad(bad),
                    false,
                    Some(&std::collections::HashSet::new()),
                    &[],
                    &|_| true,
                    Some(T_NOON),
                ),
                PruneDecision::KeepUnknown,
                "{bad:?} must keep"
            );
        }
    }

    #[test]
    fn a_fresh_member_less_squad_is_never_pruned() {
        let live = std::collections::HashSet::new();
        // Created one second ago: recruiting, not finished.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2026-08-13T11:59:59Z"),
                false,
                Some(&live),
                &[],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::KeepUnknown
        );
    }

    #[test]
    fn the_grace_boundary_keeps_until_strictly_past() {
        let live = std::collections::HashSet::new();
        let decide = |created: &str, now: i64| {
            prune_decision_at(
                &empty_squad(created),
                false,
                Some(&live),
                &[],
                &|_| true,
                Some(now),
            )
        };
        // Exactly at the window: still kept.
        assert_eq!(
            decide("2026-08-13T12:00:00Z", T_NOON + EMPTY_SQUAD_GRACE_SECS),
            PruneDecision::KeepUnknown
        );
        // One second past: prunable.
        assert_eq!(
            decide("2026-08-13T12:00:00Z", T_NOON + EMPTY_SQUAD_GRACE_SECS + 1),
            PruneDecision::Prune
        );
    }

    #[test]
    fn a_clock_we_cannot_read_keeps_every_member_less_squad() {
        // `None` is "no clock". Without one, a fresh recruit and a finished squad
        // are the same thing, so nothing may be destroyed on the guess.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2020-01-01T00:00:00Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| true,
                None,
            ),
            PruneDecision::KeepUnknown
        );
    }

    #[test]
    fn a_vanished_origin_resolves_the_ambiguity_without_waiting() {
        // Nothing left to recruit INTO, so the clock is not needed.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2026-08-13T11:59:59Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| false,
                Some(T_NOON),
            ),
            PruneDecision::Prune
        );
    }

    #[test]
    fn a_tombstoned_member_is_evidence_and_needs_no_grace() {
        // The distinction the grace window turns on. A tombstone RECORDS that a
        // member registered and died; an empty list records nothing. Three of the
        // fifteen squads measured are all-tombstoned, and they are finished now,
        // not in an hour.
        let mut s = empty_squad("2026-08-13T11:59:59Z");
        s.members = vec![StoredMember {
            attach_id: "deadbeef".into(),
            tombstone: true,
            tab_name: None,
            cwd: None,
            worker: None,
        }];
        assert_eq!(
            prune_decision_at(
                &s,
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::Prune
        );
    }

    #[test]
    fn grace_never_overrides_liveness_or_the_name_skip() {
        // The window may only ever DELAY a prune. It must not become a path that
        // reaps something live or something the operator named.
        let mut live = std::collections::HashSet::new();
        live.insert("live0001".to_string());
        let mut s = empty_squad("2020-01-01T00:00:00Z"); // long past grace
        s.members = vec![m("live0001")];
        assert_eq!(
            prune_decision_at(&s, false, Some(&live), &[], &|_| true, Some(T_NOON)),
            PruneDecision::Keep
        );

        let named = {
            let mut n = empty_squad("2020-01-01T00:00:00Z");
            n.name = "mine".into();
            n
        };
        assert_eq!(
            prune_decision_at(
                &named,
                false,
                Some(&std::collections::HashSet::new()),
                &[],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::SkipNamed
        );

        // ONE CLOCK FOR BOTH DIRECTORY ARMS.
        //
        // A live pane protects a member-less squad only while it is young enough
        // for that pane to plausibly BE its unrecorded worker. Past grace it does
        // not, and this assertion is the one that changed: the pane arm used to
        // be unconditional, so a squad weeks old stayed immortal while any
        // session ran in the same repo. Measured here, that held six of them.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2020-01-01T00:00:00Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &["/alive/child".to_string()],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::Prune
        );
        // ...and WITHIN grace the same pane still protects it, so the arm is
        // aged, not deleted. Without this pair, "past grace prunes" would also
        // pass against a predicate that ignored panes entirely.
        assert_eq!(
            prune_decision_at(
                &empty_squad("2026-08-13T11:59:00Z"),
                false,
                Some(&std::collections::HashSet::new()),
                &["/alive/child".to_string()],
                &|_| true,
                Some(T_NOON),
            ),
            PruneDecision::Keep
        );
    }

    #[test]
    fn prune_decision_delegates_to_the_clocked_form() {
        // One predicate, two entry points. The clockless wrapper must not drift
        // into a second opinion.
        let live = std::collections::HashSet::new();
        let s = empty_squad("2020-01-01T00:00:00Z");
        assert_eq!(
            prune_decision(&s, false, Some(&live), &[], &|_| true),
            prune_decision_at(&s, false, Some(&live), &[], &|_| true, None)
        );
    }

    #[test]
    fn prune_removes_only_prunable_and_preserves_lifecycle() {
        let _s = Scratch::new("prune");
        upsert("", "dead", &["/gone".into()], &[m("deadbeef")]).unwrap();
        upsert("", "kept", &["/survives".into()], &[m("deadbeef")]).unwrap();
        upsert("named", "", &["/gone".into()], &[m("deadbeef")]).unwrap();
        assert!(matches!(
            begin_external_stop("cafef00d", "x", "/t").unwrap(),
            LifecycleCas::Committed { generation: 1 }
        ));

        let live = std::collections::HashSet::<String>::new(); // nothing live
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|p| p == "/survives"),
            Some(&live),
        )
        .unwrap();

        // BOTH unnamed squads go. `kept` has a surviving origin, and that used to
        // save it; a squad whose every member is dead is finished wherever its
        // directory happens to live.
        assert_eq!(
            outcome.removed_count(),
            2,
            "a surviving origin no longer keeps a squad whose members are all dead"
        );
        let mut removed_keys: Vec<&str> = outcome.removed.iter().map(|r| r.key.as_str()).collect();
        removed_keys.sort_unstable();
        assert_eq!(
            removed_keys,
            vec!["dead", "kept"],
            "the receipt names the squads actually removed"
        );
        assert_eq!(
            outcome.skipped_named, 1,
            "the named squad is counted skip-named"
        );

        let after = load();
        assert!(
            !after.squads.iter().any(|s| s.key == "dead"),
            "prunable squad gone"
        );
        assert!(
            !after.squads.iter().any(|s| s.key == "kept"),
            "a surviving origin does not save a squad whose members are all dead"
        );
        assert!(
            after.squads.iter().any(|s| s.name == "named"),
            "named squad kept"
        );
        assert_eq!(
            after.external_lifecycle.len(),
            1,
            "external_lifecycle preserved byte-for-byte across a prune"
        );
    }

    fn tomb(id: &str) -> StoredMember {
        StoredMember {
            attach_id: id.into(),
            tombstone: true,
            tab_name: None,
            cwd: None,
            worker: None,
        }
    }

    #[test]
    fn member_reap_drops_tombstoned_members_a_live_member_keeps_the_squad_alive() {
        // AC4-HP: a squad kept by one live member still has its tombstoned
        // members reaped in the same pass, so a dead worker beside a live one
        // does not synthesize a `cc-` row forever.
        let _s = Scratch::new("member-reap-hp");
        upsert(
            "",
            "keeper",
            &[],
            &[m("11111111"), tomb("deadbee1"), tomb("deadbee2")],
        )
        .unwrap();

        let mut live = std::collections::HashSet::new();
        live.insert("11111111".to_string());
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|_| true),
            Some(&live),
        )
        .unwrap();

        assert_eq!(outcome.removed_count(), 0, "the squad itself survives");
        assert_eq!(outcome.members_reaped, 2);
        let after = load();
        assert_eq!(after.squads.len(), 1);
        assert_eq!(after.squads[0].members.len(), 1);
        assert_eq!(after.squads[0].members[0].attach_id, "11111111");
    }

    #[test]
    fn member_reap_does_nothing_when_liveness_is_unknown() {
        // AC4-EDGE: the liveness query failed (`live` is `None`). No member is
        // removed - same fail-safe direction `KeepUnknown` takes for whole
        // squads.
        let _s = Scratch::new("member-reap-edge");
        upsert(
            "",
            "keeper",
            &[],
            &[m("11111111"), tomb("deadbee1"), tomb("deadbee2")],
        )
        .unwrap();

        let outcome = prune(|sq| prune_decision(sq, false, None, &[], &|_| true), None).unwrap();

        assert_eq!(outcome.removed_count(), 0);
        assert_eq!(outcome.members_reaped, 0, "an unknown roster reaps nothing");
        let after = load();
        assert_eq!(after.squads[0].members.len(), 3);
    }

    #[test]
    fn member_reap_to_zero_does_not_get_double_pruned_in_the_same_pass() {
        // AC4-COV: a NAMED squad skipped by `include_named` policy still has
        // its tombstoned members reaped, and that can reap it to zero members
        // in this call. `decide` already ran (SkipNamed) against the ORIGINAL
        // member list before the reap, so this pass must not turn around and
        // treat the now-empty squad as prunable under the empty-squad grace
        // arm - that only happens on a LATER call, against fresh state.
        let _s = Scratch::new("member-reap-cov");
        upsert(
            "archived-crew",
            "",
            &[],
            &[tomb("deadbee1"), tomb("deadbee2")],
        )
        .unwrap();

        let live = std::collections::HashSet::<String>::new(); // nothing live
        let outcome = prune(
            |sq| prune_decision(sq, false, Some(&live), &[], &|_| true),
            Some(&live),
        )
        .unwrap();

        assert_eq!(
            outcome.removed_count(),
            0,
            "SkipNamed never proposes the squad for removal"
        );
        assert_eq!(outcome.skipped_named, 1);
        assert_eq!(outcome.members_reaped, 2);
        let after = load();
        assert_eq!(
            after.squads.len(),
            1,
            "the squad row survives with zero members, not pruned in this pass"
        );
        assert!(after.squads[0].members.is_empty());
    }
}
