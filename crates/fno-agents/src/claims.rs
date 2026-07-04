//! Native work-claim substrate: a second implementation of the lockfile
//! protocol owned by `cli/src/fno/claims/` (Python stays the reference
//! implementation and the only CLI surface).
//!
//! Scope is consumer-driven: `acquire` / `release` / `status` plus the
//! liveness classifier — exactly what the daemon/adopt/drive/stream-worker
//! call sites need. Everything else (`list`, `refresh`, `force-release`,
//! lane slots) remains Python-only.
//!
//! Protocol parity is the contract, not just passing tests. Source of truth:
//! `cli/src/fno/claims/{types,io,core,staleness}.py` and
//! `docs/architecture/coordination.md`. Load-bearing wire details a second
//! implementation must reproduce exactly:
//!
//! - lockfile path: `<root>/.fno/claims/<percent-encoded-key>.lock`, uppercase
//!   hex, safe set `[A-Za-z0-9._~-]` (Python `quote(key, safe="")`);
//! - YAML mapping with `expires_at` OMITTED (never null) for PID-liveness
//!   claims; readers ignore unknown fields and treat `schema_version > 1`,
//!   non-mapping roots, and parse failures as Corrupted;
//! - atomic create = temp file + `link(2)` publish (EEXIST = held), replace =
//!   temp file + `rename(2)`, both in the claims directory itself;
//! - stale recovery serialized under the `<lockfile-name>.recovery.d` mkdir
//!   mutex; a waiter that times out retries acquire — it NEVER rmdirs a held
//!   mutex (stealing would reintroduce the double-winner TOCTOU);
//! - stale claims are archived by rename to `.expired/<enc>.<now_ms>.lock`,
//!   never unlinked;
//! - hybrid liveness: an expired-TTL claim whose recorded pid is a live
//!   process on this host is still LIVE; PID-reuse is detected by comparing
//!   the process create time (epoch ms) against `acquired_at`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// On-disk schema version this implementation reads and writes. Readers
/// refuse anything newer rather than guess at a future writer's semantics.
pub const SCHEMA_VERSION: u32 = 1;
/// Raw key cap (mirrors `types.MAX_KEY_LENGTH`).
pub const MAX_KEY_LENGTH: usize = 256;
/// Encoded-filename cap (mirrors `types.MAX_ENCODED_FILENAME_BYTES`):
/// 240 + ".lock" = 245 bytes, under every mainstream fs's 255-byte limit.
pub const MAX_ENCODED_FILENAME_BYTES: usize = 240;
/// TTL bounds in ms (mirrors `types.MIN_TTL_MS` / `types.MAX_TTL_MS`).
pub const MIN_TTL_MS: i64 = 60_000;
pub const MAX_TTL_MS: i64 = 86_400_000;

const CLAIMS_DIRNAME: &str = ".fno/claims";
const EXPIRED_SUBDIR: &str = ".expired";

/// Recovery-mutex wait: poll cadence + deadline (mirrors core.py's 20ms/5s).
const RECOVERY_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RECOVERY_LOCK_MAX_WAIT: Duration = Duration::from_secs(5);
/// Bounded retry for gone-away / lost-recovery races. Python recurses
/// unboundedly here; a bound is an accepted divergence — hitting it means
/// pathological churn and every Rust caller is fail-open.
const ACQUIRE_MAX_ATTEMPTS: usize = 5;

/// Classification of a key's current state (mirrors `types.ClaimState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    /// No claim file exists.
    Free,
    /// Claim exists and its holder is verifiably alive.
    Live,
    /// TTL unexpired but the holder is NOT provably alive (dead/replaced pid).
    /// A respawned worker whose supervisor pid died reads here: the TTL still
    /// protects the claim, so it is treated like `Live` for acquire/dispatch
    /// (never stolen) - only TTL expiry (-> `Stale`) frees it.
    Suspect,
    /// Claim exists but the holder is dead/expired (recoverable).
    Stale,
    /// Claim file present but unreadable (parse/schema failure).
    Corrupted,
}

impl ClaimState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClaimState::Free => "free",
            ClaimState::Live => "live",
            ClaimState::Suspect => "suspect",
            ClaimState::Stale => "stale",
            ClaimState::Corrupted => "corrupted",
        }
    }
}

/// On-disk claim record (mirrors `types.Claim` / `Claim.to_yaml_dict`).
///
/// Field order here IS the YAML output order (serde preserves struct order),
/// matching the Python writer: schema_version, key, holder, acquired_at, pid,
/// host, then the optional tail. `expires_at: None` must serialize as an
/// ABSENT key, never `expires_at: null` — the absence is the PID-liveness
/// marker (protocol invariant).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimRecord {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub key: String,
    pub holder: String,
    /// Epoch milliseconds, UTC.
    pub acquired_at: i64,
    pub pid: i32,
    pub host: String,
    /// Epoch ms of TTL expiry; absent (and treated same as null on read) for
    /// PID-liveness claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Opaque; preserved byte-for-byte through idempotent re-acquires.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// Options for [`acquire`]. `pid` defaults to the calling process — which,
/// natively, is the long-lived daemon/worker rather than a transient CLI
/// subprocess, so the claim is live from birth (closes the acquire-to-reanchor
/// stale window the shelled implementation had).
#[derive(Debug, Default, Clone)]
pub struct AcquireOpts {
    pub pid: Option<u32>,
    pub ttl_ms: Option<i64>,
    pub reason: Option<String>,
    pub metadata: Option<Map<String, Value>>,
    /// Explicit claims ROOT (the dir that contains `.fno/claims`). `None`
    /// resolves by key prefix: global-id keys (`node:`/`dispatch:`/
    /// `reconcile:`/`session:`) route to `$FNO_CLAIMS_ROOT` (else `$HOME`).
    pub root: Option<PathBuf>,
    /// Where audit events land (the dir containing `.fno/events.jsonl`).
    /// `None` = current working directory, matching the Python emitter.
    pub events_dir: Option<PathBuf>,
}

/// Outcome of [`acquire`] (mirrors core.py's acquire/`ClaimHeldByOther`).
#[derive(Debug, Clone, PartialEq)]
pub enum AcquireOutcome {
    /// Fresh acquire, idempotent re-acquire, or stale reclaim.
    Acquired(ClaimRecord),
    /// A live claim is held by a different holder.
    HeldByOther {
        holder: String,
        pid: i32,
        host: String,
    },
    /// Validation / io / corruption error. Callers keep their fail-open
    /// posture (this maps to the historical `ClaimOutcome::Unavailable`).
    Error(String),
}

// ---------------------------------------------------------------------------
// Key encoding + path resolution
// ---------------------------------------------------------------------------

/// Percent-encode a key for use as a filename. Byte-parity with Python's
/// `urllib.parse.quote(key, safe="")`: every byte NOT in `[A-Za-z0-9._~-]`
/// becomes `%XX` with UPPERCASE hex (a lowercase encoder would produce a
/// different filename and silently fork the lock).
pub fn encode_key(key: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'~' | b'-' => {
                out.push(b as char)
            }
            // Direct hex-nibble push: avoids the `format!` machinery + a heap
            // allocation per escaped byte on this per-path-resolution hot path.
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xF) as usize] as char);
            }
        }
    }
    out
}

/// Claim prefixes whose identifier is globally unique (mirrors
/// `io._GLOBAL_ID_PREFIXES`): these coordinate across worktrees/repos via the
/// global root, never a cwd-local dir.
const GLOBAL_ID_PREFIXES: &[&str] = &["node", "dispatch", "reconcile", "session"];

/// The global claims ROOT: `$FNO_CLAIMS_ROOT`, else `$HOME`. A set-but-EMPTY
/// env value is UNSET (falls to `$HOME`) — Python's `os.environ.get` returns
/// the empty string, which is falsy there; resolving it here as a real path
/// would silently fork the claims dir (the drive.rs empty-is-unset lesson).
pub fn global_claims_root() -> Option<PathBuf> {
    global_claims_root_from(
        std::env::var_os("FNO_CLAIMS_ROOT"),
        std::env::var_os("HOME"),
    )
}

/// Testable core of [`global_claims_root`]: env values are explicit so the
/// empty-is-unset contract is exercised without mutating process-global env.
pub fn global_claims_root_from(
    claims_root: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: OsString| (!v.is_empty()).then_some(v);
    claims_root
        .and_then(non_empty)
        .or_else(|| home.and_then(non_empty))
        .map(PathBuf::from)
}

/// Resolve the claims ROOT for `key` by prefix (mirrors `io.claims_root_for`):
/// `<prefix>:<id>` with a global-id prefix routes to the global root; a
/// colon-less key or unrecognized prefix returns `None` (caller must pass an
/// explicit root — the Python canonical-repo-root fallback is deliberately
/// not ported; no Rust caller needs it).
pub fn claims_root_for(key: &str) -> Option<PathBuf> {
    match key.split_once(':') {
        Some((prefix, _)) if GLOBAL_ID_PREFIXES.contains(&prefix) => global_claims_root(),
        _ => None,
    }
}

fn claims_dir(key: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(r) = root {
        return Ok(r.join(CLAIMS_DIRNAME));
    }
    match claims_root_for(key) {
        Some(r) => Ok(r.join(CLAIMS_DIRNAME)),
        None => Err(format!(
            "no claims root for key {key:?}: not a global-id prefix and no explicit root given"
        )),
    }
}

/// The canonical lockfile path for a claim key.
pub fn claim_path(key: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    Ok(claims_dir(key, root)?.join(format!("{}.lock", encode_key(key))))
}

// ---------------------------------------------------------------------------
// Time, host, and process liveness
// ---------------------------------------------------------------------------

/// Current UTC time as epoch milliseconds (mirrors `staleness.now_ms`).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `gethostname(2)`, matching Python `socket.gethostname()`. Empty string on
/// failure (which can never equal a recorded non-empty host, so an unreadable
/// hostname fails toward "not live" — recoverable, like Python's posture).
fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Process create time in EPOCH MILLISECONDS, or `None` if the pid is gone or
/// uninspectable (permission denied counts as dead: a holder we cannot
/// inspect is one we cannot validate — fail toward recoverable, matching
/// psutil's NoSuchProcess/AccessDenied handling).
///
/// This is a SIBLING of `daemon::process_start_time`, not a reuse: that
/// helper returns platform-native units (Linux ticks / macOS µs) compared
/// only for equality against itself; the claims protocol needs an absolute
/// epoch-ms value comparable against `acquired_at`.
#[cfg(target_os = "macos")]
pub fn process_create_time_ms(pid: i32) -> Option<i64> {
    use std::mem;
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: buffer is a zeroed proc_bsdinfo of exactly `size` bytes; a
    // partial fill means gone / not introspectable -> None.
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
    Some((info.pbi_start_tvsec as i64) * 1000 + (info.pbi_start_tvusec as i64) / 1000)
}

/// Linux: epoch create time = `btime` (epoch seconds, from `/proc/stat`) plus
/// `starttime` (field 22 of `/proc/<pid>/stat`, clock ticks since boot) over
/// `sysconf(_SC_CLK_TCK)` — the same computation psutil performs. Sub-second
/// skew vs psutil's float math is tolerable: the comparison is directional
/// and real holders start well before they claim.
#[cfg(target_os = "linux")]
pub fn process_create_time_ms(pid: i32) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) can contain spaces/parens; split on the LAST ')'.
    let after = stat.rsplit_once(')')?.1;
    let starttime: i64 = after.split_whitespace().nth(19)?.parse().ok()?;
    let btime = linux_boot_time_s()?;
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if tck <= 0 {
        return None;
    }
    Some(btime * 1000 + starttime * 1000 / tck as i64)
}

#[cfg(target_os = "linux")]
fn linux_boot_time_s() -> Option<i64> {
    // btime (boot epoch seconds) is constant for the life of the host, so cache
    // it: process_create_time_ms is on the claim status/acquire hot path and
    // re-reading /proc/stat every call is wasted I/O.
    static BTIME: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    *BTIME.get_or_init(|| {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        for line in stat.lines() {
            if let Some(rest) = line.strip_prefix("btime ") {
                return rest.trim().parse().ok();
            }
        }
        None
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_create_time_ms(_pid: i32) -> Option<i64> {
    None
}

/// Is the claim's holder verifiably running? (mirrors `staleness.is_live`)
/// False when: cross-host, pid gone/uninspectable, or the current occupant of
/// the pid slot started AFTER the claim was filed (PID reuse).
fn is_live(rec: &ClaimRecord) -> bool {
    if rec.host != hostname() {
        return false;
    }
    match process_create_time_ms(rec.pid) {
        Some(create_ms) => create_ms <= rec.acquired_at,
        None => false,
    }
}

fn is_expired(rec: &ClaimRecord, now: i64) -> bool {
    match rec.expires_at {
        Some(exp) => now >= exp,
        None => false,
    }
}

/// Compose liveness + expiry into a state (mirrors `staleness.classify`,
/// INCLUDING the hybrid arm: an expired-TTL claim whose recorded pid is a
/// live process on this host is still LIVE — a suspended-but-alive session
/// must not have its claim reclaimed by a peer).
///
/// SUSPECT arm (x-ba4b): a TTL claim still inside its window whose recorded pid
/// is NOT a live process reads `Suspect`, not `Live`. Dead-pid-but-unexpired is
/// the respawned-worker case (supervisor pid died, session lives on): the TTL
/// keeps protecting the slot, so acquire/dispatch treat it like `Live` (never
/// steal), but the distinct state lets init/dispatch branch on it. Only TTL
/// expiry frees the claim (-> `Stale`); pid death alone never does.
pub fn classify(rec: &ClaimRecord, now: Option<i64>) -> ClaimState {
    let now = now.unwrap_or_else(now_ms);
    if is_expired(rec, now) {
        return if is_live(rec) {
            ClaimState::Live
        } else {
            ClaimState::Stale
        };
    }
    if rec.expires_at.is_none() {
        return if is_live(rec) {
            ClaimState::Live
        } else {
            ClaimState::Stale
        };
    }
    // TTL claim, still inside its window: live pid => Live, dead/replaced pid
    // => Suspect (TTL-protected, not stealable).
    if is_live(rec) {
        ClaimState::Live
    } else {
        ClaimState::Suspect
    }
}

// ---------------------------------------------------------------------------
// YAML read/write + atomic file ops
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ReadError {
    /// File disappeared between decision and read.
    GoneAway,
    /// Unparseable YAML, non-mapping root, schema violation, or io error.
    Corrupted(String),
}

fn serialize_claim(rec: &ClaimRecord) -> Result<String, String> {
    serde_yaml_ng::to_string(rec).map_err(|e| format!("claim YAML serialize failed: {e}"))
}

fn parse_claim_str(text: &str) -> Result<ClaimRecord, ReadError> {
    let rec: ClaimRecord = serde_yaml_ng::from_str(text)
        .map_err(|e| ReadError::Corrupted(format!("claim parse/schema failed: {e}")))?;
    if rec.schema_version > SCHEMA_VERSION {
        return Err(ReadError::Corrupted(format!(
            "claim schema_version={} > supported={SCHEMA_VERSION}; refusing to read from a newer writer",
            rec.schema_version
        )));
    }
    if rec.key.is_empty() || rec.holder.is_empty() {
        return Err(ReadError::Corrupted(
            "claim key/holder must be non-empty".into(),
        ));
    }
    Ok(rec)
}

fn read_claim_file(path: &Path) -> Result<ClaimRecord, ReadError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::GoneAway),
        Err(e) => return Err(ReadError::Corrupted(format!("claim read failed: {e}"))),
    };
    parse_claim_str(&text)
}

enum CreateError {
    /// The target path already exists (a concurrent winner published first).
    AlreadyHeld,
    Io(String),
}

/// Atomically create `path` with `content`, failing if it already exists.
/// Temp file in the SAME directory, then `link(2)` into place: atomic publish
/// with EEXIST loser detection, and a concurrent reader sees either no file
/// or a fully-written one — never a created-but-empty file that would parse
/// as Corrupted. Creates the parent dir on ENOENT and retries exactly once;
/// other errors (ENOSPC, EACCES, ...) surface with no partial file at `path`.
fn atomic_create_exclusive(path: &Path, content: &str) -> Result<(), CreateError> {
    let parent = match path.parent() {
        Some(p) => p,
        None => return Err(CreateError::Io("claim path has no parent".into())),
    };
    match create_via_link(parent, path, content) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(CreateError::AlreadyHeld),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent).map_err(|e| CreateError::Io(e.to_string()))?;
            match create_via_link(parent, path, content) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(CreateError::AlreadyHeld)
                }
                Err(e) => Err(CreateError::Io(e.to_string())),
            }
        }
        Err(e) => Err(CreateError::Io(e.to_string())),
    }
}

fn create_via_link(parent: &Path, path: &Path, content: &str) -> std::io::Result<()> {
    // pid + coarse clock alone can collide across threads in this process (same
    // nanosecond bucket), and a colliding temp name makes the second thread's
    // `create_new` fail AlreadyExists -> mis-mapped to a FALSE `AlreadyHeld`
    // lock failure. A process-unique counter guarantees distinct temp names.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = parent.join(format!(
        ".claim-tmp-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        // No fsync: once write returns, a same-fs reader sees the content via
        // the page cache — all the hardlink publish needs (a lock file does
        // not require crash durability).
        f.write_all(content.as_bytes())?;
    }
    let res = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    res
}

/// Replace `path` with `content` via write-temp + rename (idempotent
/// re-acquire path). Temp in the same directory so the rename is atomic;
/// tmp is cleaned up on any failure between write and rename.
fn atomic_replace(path: &Path, content: &str) -> Result<(), String> {
    // Counter (not just pid): two threads replacing the SAME claim path (e.g.
    // concurrent same-key idempotent re-acquires) would otherwise share a temp
    // name and clobber each other. Uniqueness makes each replace independent.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "lock.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let write = std::fs::write(&tmp, content)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| e.to_string());
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Archive a stale claim into `.expired/` by RENAME (never unlink: the
/// forensic trail must survive). A missing source is success (another process
/// archived first); a real rename/mkdir failure is PROPAGATED so the caller
/// fails fast with a clear diagnostic instead of looping until the generic
/// contention-retry ceiling (a persistently un-archivable stale file would
/// otherwise exhaust every acquire attempt with a misleading error).
fn archive_claim(path: &Path, ts_ms: i64) -> std::io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid claim path for archive",
        ));
    };
    let stem = name.strip_suffix(".lock").unwrap_or(name);
    let archive_dir = parent.join(EXPIRED_SUBDIR);
    std::fs::create_dir_all(&archive_dir)?;
    match std::fs::rename(path, archive_dir.join(format!("{stem}.{ts_ms}.lock"))) {
        Ok(()) => Ok(()),
        // Source gone: another actor archived it first — success.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Audit events (Branch-A envelope, exact parity with fno.claims.events)
// ---------------------------------------------------------------------------

/// Best-effort audit append to `<events_dir>/.fno/events.jsonl` using the
/// SAME envelope the Python emitter writes: `{ts, type, source: "abi-loop",
/// data}` — so an operator reading the log (or `fno event audit`) sees the
/// identical record regardless of which implementation performed the
/// operation. Deliberately NOT the crate's Branch-B `EventEmitter`: that
/// envelope is kind-flat with a 500-byte payload cap, either of which would
/// break record parity for these events.
///
/// Serializes on the cross-language `events.jsonl.lock.d` mkdir mutex (the
/// convention `fno.events.append_event` and the shell writers share), with a
/// short bounded wait: this runs on daemon hot paths, so a wedged lock means
/// we log and skip rather than block. The lockfile write is authoritative;
/// this log is observability only.
fn emit_claim_event(events_dir: Option<&Path>, type_name: &str, data: Map<String, Value>) {
    let base = events_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let events_path = base.join(".fno/events.jsonl");
    let event = json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "type": type_name,
        "source": "abi-loop",
        "data": Value::Object(data),
    });
    if let Err(e) = append_event_line(&events_path, &event) {
        eprintln!("claims: failed to emit {type_name:?}: {e}");
    }
}

fn append_event_line(events_path: &Path, event: &Value) -> Result<(), String> {
    if let Some(parent) = events_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let lock_dir = events_path.with_file_name(format!(
        "{}.lock.d",
        events_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "events.jsonl".into())
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::create_dir(&lock_dir) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    return Err(format!("events.jsonl lock timeout: {}", lock_dir.display()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let res = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(events_path)
        .and_then(|mut f| writeln!(f, "{event}"))
        .map_err(|e| e.to_string());
    let _ = std::fs::remove_dir(&lock_dir);
    res
}

/// Shared data fields for claim events (mirrors `events._common`, including
/// the explicit `expires_at: null` for PID-liveness claims — the EVENT payload
/// carries null where the LOCKFILE omits the key; that asymmetry is Python's).
fn common_event_data(rec: &ClaimRecord) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".into(), Value::String(rec.key.clone()));
    m.insert("holder".into(), Value::String(rec.holder.clone()));
    m.insert("pid".into(), Value::Number(rec.pid.into()));
    m.insert("host".into(), Value::String(rec.host.clone()));
    m.insert("acquired_at".into(), Value::Number(rec.acquired_at.into()));
    m.insert(
        "expires_at".into(),
        rec.expires_at.map(Value::from).unwrap_or(Value::Null),
    );
    m
}

// ---------------------------------------------------------------------------
// Verbs: acquire / release / status
// ---------------------------------------------------------------------------

fn validate_inputs(key: &str, holder: &str, ttl_ms: Option<i64>) -> Result<(), String> {
    if key.is_empty() {
        return Err("key must be non-empty".into());
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(format!(
            "key length {} exceeds MAX_KEY_LENGTH={MAX_KEY_LENGTH}",
            key.len()
        ));
    }
    // Raw length under the cap does not bound the ENCODED filename: reserved
    // bytes expand 3x (worst case). Check the encoded form explicitly.
    let encoded_len = encode_key(key).len();
    if encoded_len > MAX_ENCODED_FILENAME_BYTES {
        return Err(format!(
            "URL-encoded key length {encoded_len} exceeds MAX_ENCODED_FILENAME_BYTES={MAX_ENCODED_FILENAME_BYTES}"
        ));
    }
    if holder.is_empty() {
        return Err("holder must be non-empty".into());
    }
    if let Some(ttl) = ttl_ms {
        if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl) {
            return Err(format!(
                "ttl_ms={ttl} out of range [{MIN_TTL_MS}, {MAX_TTL_MS}]"
            ));
        }
    }
    Ok(())
}

fn make_claim(key: &str, holder: &str, opts: &AcquireOpts) -> ClaimRecord {
    let acquired = now_ms();
    ClaimRecord {
        schema_version: SCHEMA_VERSION,
        key: key.into(),
        holder: holder.into(),
        acquired_at: acquired,
        pid: opts.pid.unwrap_or_else(std::process::id) as i32,
        host: hostname(),
        expires_at: opts.ttl_ms.map(|ttl| acquired + ttl),
        reason: opts.reason.clone(),
        metadata: opts.metadata.clone().unwrap_or_default(),
    }
}

/// Try to acquire a claim on `key` for `holder` (mirrors `core.acquire_claim`).
///
/// Resolution order when the lockfile already exists:
///   1. same holder -> idempotent re-acquire (rewrite with refreshed
///      pid/host/acquired_at, metadata replaced by the new call's);
///   2. not live -> stale recovery under the `.recovery.d` mkdir mutex
///      (archive to `.expired/`, exclusive-create the new claim);
///   3. live other -> `HeldByOther`.
///
/// Validation failures return `Error` before any filesystem write. The
/// gone-away race (claim released between collision and read) retries from
/// the top, bounded at [`ACQUIRE_MAX_ATTEMPTS`].
pub fn acquire(key: &str, holder: &str, opts: AcquireOpts) -> AcquireOutcome {
    if let Err(e) = validate_inputs(key, holder, opts.ttl_ms) {
        return AcquireOutcome::Error(e);
    }
    let path = match claim_path(key, opts.root.as_deref()) {
        Ok(p) => p,
        Err(e) => return AcquireOutcome::Error(e),
    };
    let events_dir = opts.events_dir.clone();

    for _attempt in 0..ACQUIRE_MAX_ATTEMPTS {
        let new_claim = make_claim(key, holder, &opts);
        let payload = match serialize_claim(&new_claim) {
            Ok(p) => p,
            Err(e) => return AcquireOutcome::Error(e),
        };

        match atomic_create_exclusive(&path, &payload) {
            Ok(()) => {
                emit_claim_event(
                    events_dir.as_deref(),
                    "claim_acquired",
                    acquired_event_data(&new_claim),
                );
                return AcquireOutcome::Acquired(new_claim);
            }
            Err(CreateError::AlreadyHeld) => {}
            Err(CreateError::Io(e)) => return AcquireOutcome::Error(e),
        }

        // Path exists; classify the existing holder.
        let existing = match read_claim_file(&path) {
            Ok(rec) => rec,
            Err(ReadError::GoneAway) => continue, // released under us; retry
            Err(ReadError::Corrupted(e)) => {
                // Refuse to reclaim what we cannot verify; leave the file for
                // `fno claim force-release`.
                return AcquireOutcome::Error(e);
            }
        };

        if existing.holder == holder {
            return idempotent_reacquire(
                &path,
                key,
                holder,
                &opts,
                &existing,
                events_dir.as_deref(),
            );
        }

        // Suspect (TTL-unexpired, dead pid) refuses exactly like Live: the TTL
        // still protects a respawned worker's slot, so we never reclaim it.
        if !matches!(
            classify(&existing, None),
            ClaimState::Live | ClaimState::Suspect
        ) {
            match recover_stale(&path, key, holder, &opts, events_dir.as_deref()) {
                RecoverResult::Done(outcome) => return outcome,
                RecoverResult::Retry => continue,
            }
        } else {
            return AcquireOutcome::HeldByOther {
                holder: existing.holder,
                pid: existing.pid,
                host: existing.host,
            };
        }
    }
    AcquireOutcome::Error(format!(
        "acquire gave up after {ACQUIRE_MAX_ATTEMPTS} contention retries on {key:?}"
    ))
}

fn acquired_event_data(rec: &ClaimRecord) -> Map<String, Value> {
    let mut data = common_event_data(rec);
    if let Some(r) = &rec.reason {
        data.insert("reason".into(), Value::String(r.clone()));
    }
    data
}

fn idempotent_reacquire(
    path: &Path,
    key: &str,
    holder: &str,
    opts: &AcquireOpts,
    existing: &ClaimRecord,
    events_dir: Option<&Path>,
) -> AcquireOutcome {
    let refreshed = make_claim(key, holder, opts);
    let payload = match serialize_claim(&refreshed) {
        Ok(p) => p,
        Err(e) => return AcquireOutcome::Error(e),
    };
    if let Err(e) = atomic_replace(path, &payload) {
        return AcquireOutcome::Error(e);
    }
    let mut data = common_event_data(&refreshed);
    data.insert(
        "previous_acquired_at".into(),
        Value::Number(existing.acquired_at.into()),
    );
    emit_claim_event(events_dir, "claim_idempotent_reacquired", data);
    AcquireOutcome::Acquired(refreshed)
}

enum RecoverResult {
    Done(AcquireOutcome),
    /// Another worker holds (or held) the recovery mutex, or a third worker
    /// won a create race: retry the whole acquire.
    Retry,
}

/// Stale-claim recovery under the shared mkdir mutex. The mutex NAME
/// (`<lockfile-name>.recovery.d`) and the no-steal rule are wire protocol:
/// they are how a Python worker and this implementation serialize recovery of
/// the same claim. A waiter whose deadline expires retries acquire — it never
/// rmdirs a mutex it does not hold (the holder may still be mid-archive).
fn recover_stale(
    path: &Path,
    key: &str,
    holder: &str,
    opts: &AcquireOpts,
    events_dir: Option<&Path>,
) -> RecoverResult {
    let recovery_lock = path.with_file_name(format!(
        "{}.recovery.d",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    match std::fs::create_dir(&recovery_lock) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another worker is doing recovery. Wait briefly, then retry from
            // the top: the recovering worker either succeeded (we then see
            // live-other) or failed (we get another shot).
            wait_for_recovery_release(&recovery_lock, RECOVERY_LOCK_MAX_WAIT);
            return RecoverResult::Retry;
        }
        // The claims dir itself vanished (or another io failure): retry from
        // the top, where exclusive-create will recreate the parent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RecoverResult::Retry,
        Err(e) => return RecoverResult::Done(AcquireOutcome::Error(e.to_string())),
    }

    // Inside the mutex: rmdir on ALL paths out.
    let result = recover_stale_locked(path, key, holder, opts, events_dir);
    let _ = std::fs::remove_dir(&recovery_lock);
    result
}

/// The critical section of [`recover_stale`]: re-read (the holder may have
/// changed or vanished while we grabbed the mutex), re-classify, then
/// archive + exclusive-create.
fn recover_stale_locked(
    path: &Path,
    key: &str,
    holder: &str,
    opts: &AcquireOpts,
    events_dir: Option<&Path>,
) -> RecoverResult {
    let new_claim = make_claim(key, holder, opts);
    let payload = match serialize_claim(&new_claim) {
        Ok(p) => p,
        Err(e) => return RecoverResult::Done(AcquireOutcome::Error(e)),
    };

    let existing = match read_claim_file(path) {
        Err(ReadError::GoneAway) => {
            // Vanished while we held the mutex — someone released cleanly.
            // Create at the empty path; a third worker racing into create
            // between the gone-away read and this call sends us back around.
            return match atomic_create_exclusive(path, &payload) {
                Ok(()) => {
                    emit_claim_event(
                        events_dir,
                        "claim_acquired",
                        acquired_event_data(&new_claim),
                    );
                    RecoverResult::Done(AcquireOutcome::Acquired(new_claim))
                }
                Err(CreateError::AlreadyHeld) => RecoverResult::Retry,
                Err(CreateError::Io(e)) => RecoverResult::Done(AcquireOutcome::Error(e)),
            };
        }
        Err(ReadError::Corrupted(e)) => return RecoverResult::Done(AcquireOutcome::Error(e)),
        Ok(rec) => rec,
    };

    if existing.holder == holder {
        // Raced into the idempotent path while grabbing the mutex.
        return RecoverResult::Done(idempotent_reacquire(
            path, key, holder, opts, &existing, events_dir,
        ));
    }

    if matches!(
        classify(&existing, None),
        ClaimState::Live | ClaimState::Suspect
    ) {
        // Raced — now it's live (or a TTL-protected suspect); back off, no steal.
        return RecoverResult::Done(AcquireOutcome::HeldByOther {
            holder: existing.holder,
            pid: existing.pid,
            host: existing.host,
        });
    }

    // Still stale: archive + recreate atomically (under the mutex). A real
    // archive failure (perms / disk) is surfaced, not retried into the generic
    // contention ceiling.
    if let Err(e) = archive_claim(path, now_ms()) {
        return RecoverResult::Done(AcquireOutcome::Error(format!(
            "failed to archive stale claim: {e}"
        )));
    }
    match atomic_create_exclusive(path, &payload) {
        Ok(()) => {
            let mut data = common_event_data(&new_claim);
            data.insert(
                "previous_holder".into(),
                Value::String(existing.holder.clone()),
            );
            data.insert("previous_pid".into(), Value::Number(existing.pid.into()));
            emit_claim_event(events_dir, "claim_stale_reclaimed", data);
            RecoverResult::Done(AcquireOutcome::Acquired(new_claim))
        }
        Err(CreateError::AlreadyHeld) => RecoverResult::Retry,
        Err(CreateError::Io(e)) => RecoverResult::Done(AcquireOutcome::Error(e)),
    }
}

/// Poll for another worker's recovery mutex to clear (mirrors
/// `core._wait_for_recovery_release`): bounded wait, then the caller retries
/// acquire regardless — never steal.
fn wait_for_recovery_release(recovery_lock: &Path, max_wait: Duration) {
    let deadline = Instant::now() + max_wait;
    while recovery_lock.exists() && Instant::now() < deadline {
        std::thread::sleep(RECOVERY_LOCK_POLL_INTERVAL);
    }
}

/// Release a claim we hold (mirrors `core.release_claim`, non-strict):
/// missing file, different holder, and corrupted file are all silent success
/// (releases are idempotent; a corrupted file is left for force-release).
pub fn release(
    key: &str,
    holder: &str,
    root: Option<&Path>,
    events_dir: Option<&Path>,
) -> Result<(), String> {
    if key.is_empty() || holder.is_empty() {
        return Err("key and holder must be non-empty".into());
    }
    let path = claim_path(key, root)?;
    let existing = match read_claim_file(&path) {
        Ok(rec) => rec,
        Err(ReadError::GoneAway) => return Ok(()),
        Err(ReadError::Corrupted(_)) => return Ok(()),
    };
    if existing.holder != holder {
        return Ok(());
    }
    let duration_ms = (now_ms() - existing.acquired_at).max(0);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    }
    let mut data = common_event_data(&existing);
    data.insert("duration_held_ms".into(), Value::Number(duration_ms.into()));
    emit_claim_event(events_dir, "claim_released", data);
    Ok(())
}

/// Inspect a single key (mirrors `core.claim_status`). Never errors: a
/// missing file (or one that vanishes mid-read) is `Free`, an unreadable one
/// is `Corrupted` with no record, and an unresolvable claims root reads as
/// `Free` (fail-open — the callers of `status` gate side effects on `Live`).
pub fn status(key: &str, root: Option<&Path>) -> (ClaimState, Option<ClaimRecord>) {
    let path = match claim_path(key, root) {
        Ok(p) => p,
        Err(_) => return (ClaimState::Free, None),
    };
    if !path.exists() {
        return (ClaimState::Free, None);
    }
    match read_claim_file(&path) {
        Ok(rec) => (classify(&rec, None), Some(rec)),
        Err(ReadError::GoneAway) => (ClaimState::Free, None),
        Err(ReadError::Corrupted(_)) => (ClaimState::Corrupted, None),
    }
}

/// Best-effort lease renewal (x-ba4b): extend a TTL claim's `expires_at`, but
/// ONLY if the on-disk holder still matches `holder`. `fno-agents loop-check`
/// calls this on every stop so a respawned worker (whose supervisor pid died)
/// keeps its claim's TTL fresh under any pid, with no separate heartbeat.
///
/// The extension re-anchors the claim's OWN original TTL span
/// (`expires_at - acquired_at`) to now, so no TTL string has to be threaded
/// through the manifest. Only `expires_at` changes — `acquired_at`, `pid`,
/// `host`, `reason`, `metadata` are preserved byte-for-byte, so PID-reuse
/// detection (`create_time <= acquired_at`) is untouched.
///
/// Returns `Ok(true)` when renewed, `Ok(false)` on a benign no-op (missing /
/// gone / corrupted claim, held by a different holder, or a PID-liveness claim
/// with no `expires_at` to extend), and `Err(_)` only on a real write failure
/// (perms/disk) — callers stay best-effort and treat any outcome as advisory.
pub fn renew(key: &str, holder: &str, root: Option<&Path>) -> Result<bool, String> {
    if key.is_empty() || holder.is_empty() {
        return Err("key and holder must be non-empty".into());
    }
    let path = claim_path(key, root)?;
    let mut existing = match read_claim_file(&path) {
        Ok(rec) => rec,
        Err(ReadError::GoneAway) => return Ok(false),
        Err(ReadError::Corrupted(_)) => return Ok(false),
    };
    if existing.holder != holder {
        return Ok(false); // not ours — never touch a peer's lease
    }
    let old_exp = match existing.expires_at {
        Some(exp) => exp,
        None => return Ok(false), // PID-liveness claim: no TTL to extend
    };
    // The claim's own TTL span, floored at MIN_TTL so a degenerate record never
    // produces a shrinking or past deadline.
    let span = (old_exp - existing.acquired_at).max(MIN_TTL_MS);
    existing.expires_at = Some(now_ms() + span);
    let payload = serialize_claim(&existing)?;
    atomic_replace(&path, &payload)?;
    Ok(true)
}

/// Process-global lock serializing every test (in ANY module) that mutates
/// `FNO_CLAIMS_ROOT` / `PATH` / `FNO_BIN`. Env vars are process-global and the
/// crate test suite runs multithreaded, so a per-module lock lets a daemon test
/// and a drive test interleave and clobber each other's env — one shared mutex
/// is the only correct serialization. `cfg(test)` sets crate-wide during
/// `cargo test`, so this is visible to every module's test code.
#[cfg(test)]
pub fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn opts_in(root: &TempDir) -> AcquireOpts {
        AcquireOpts {
            root: Some(root.path().to_path_buf()),
            events_dir: Some(root.path().to_path_buf()),
            ..Default::default()
        }
    }

    fn lockfile(root: &TempDir, key: &str) -> PathBuf {
        claim_path(key, Some(root.path())).unwrap()
    }

    fn read_events(root: &TempDir) -> Vec<Value> {
        let text =
            std::fs::read_to_string(root.path().join(".fno/events.jsonl")).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // ---- lease renewal (x-ba4b) -----------------------------------------

    fn read_claim(root: &TempDir, key: &str) -> ClaimRecord {
        read_claim_file(&lockfile(root, key)).unwrap()
    }

    #[test]
    fn renew_extends_expires_at_for_matching_holder() {
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        match acquire("node:x-renew", "target-session:me", o) {
            AcquireOutcome::Acquired(_) => {}
            other => panic!("{other:?}"),
        };
        let before = read_claim(&td, "node:x-renew").expires_at.unwrap();
        let acquired_at = read_claim(&td, "node:x-renew").acquired_at;
        // Renew re-anchors the original 120s span to now: after >=1ms elapsed
        // the new deadline is strictly later, and acquired_at is preserved.
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            renew("node:x-renew", "target-session:me", Some(td.path())),
            Ok(true)
        );
        let after = read_claim(&td, "node:x-renew");
        assert!(
            after.expires_at.unwrap() > before,
            "expected extended deadline, before={before} after={:?}",
            after.expires_at
        );
        assert_eq!(after.acquired_at, acquired_at, "acquired_at must be preserved");
    }

    #[test]
    fn renew_is_noop_for_wrong_holder() {
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        let _ = acquire("node:x-other", "target-session:owner", o);
        let before = read_claim(&td, "node:x-other").expires_at.unwrap();
        // A peer must never extend a claim it does not hold.
        assert_eq!(
            renew("node:x-other", "target-session:intruder", Some(td.path())),
            Ok(false)
        );
        assert_eq!(read_claim(&td, "node:x-other").expires_at.unwrap(), before);
    }

    #[test]
    fn renew_is_noop_for_pid_liveness_and_missing_claim() {
        let td = TempDir::new().unwrap();
        // Missing claim -> Ok(false).
        assert_eq!(
            renew("node:x-absent", "h", Some(td.path())),
            Ok(false)
        );
        // PID-liveness claim (no ttl_ms) has no expires_at to extend -> Ok(false).
        let _ = acquire("session:pidonly", "h", opts_in(&td));
        assert!(read_claim(&td, "session:pidonly").expires_at.is_none());
        assert_eq!(renew("session:pidonly", "h", Some(td.path())), Ok(false));
    }

    // ---- encoding parity (contract item 1) ------------------------------

    #[test]
    fn encode_key_matches_python_quote_safe_empty() {
        // Vectors cross-checked against urllib.parse.quote(key, safe="").
        assert_eq!(encode_key("node:ab-1234abcd"), "node%3Aab-1234abcd");
        assert_eq!(encode_key("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_key("A-Z_a.z~0"), "A-Z_a.z~0");
        // Uppercase hex: lowercase would silently fork the lock filename.
        assert_eq!(encode_key("k:v"), "k%3Av");
        // Non-ASCII percent-encodes per UTF-8 byte.
        assert_eq!(encode_key("é"), "%C3%A9");
        assert_eq!(encode_key("走"), "%E8%B5%B0");
    }

    #[test]
    fn set_but_empty_claims_root_is_unset() {
        let root = global_claims_root_from(Some(OsString::new()), Some(OsString::from("/home/x")));
        assert_eq!(root, Some(PathBuf::from("/home/x")));
        let root = global_claims_root_from(
            Some(OsString::from("/custom")),
            Some(OsString::from("/home/x")),
        );
        assert_eq!(root, Some(PathBuf::from("/custom")));
        assert_eq!(global_claims_root_from(None, None), None);
    }

    #[test]
    fn root_routing_requires_colon_and_known_prefix() {
        // A bare token equal to a prefix must NOT route globally (partition
        // semantics: a global-id key is always "<prefix>:<id>").
        assert!(claims_dir("node", None).is_err());
        assert!(claims_dir("walker:/repo/root", None).is_err());
        // Explicit root always wins.
        let dir = claims_dir("walker:/repo/root", Some(Path::new("/tmp/x"))).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/x/.fno/claims"));
    }

    // ---- validation bounds (contract item 10) ----------------------------

    #[test]
    fn validation_rejects_bad_inputs_before_any_write() {
        let td = TempDir::new().unwrap();
        let o = opts_in(&td);
        let err = |k: &str, h: &str, opts: AcquireOpts| match acquire(k, h, opts) {
            AcquireOutcome::Error(e) => e,
            other => panic!("expected Error, got {other:?}"),
        };
        assert!(err("", "h", o.clone()).contains("key must be non-empty"));
        assert!(err("k", "", o.clone()).contains("holder must be non-empty"));
        let long_key = "k".repeat(257);
        assert!(err(&long_key, "h", o.clone()).contains("MAX_KEY_LENGTH"));
        // Worst-case 3x expansion: 100 colons is 300 encoded bytes > 240.
        let expanding = ":".repeat(100);
        assert!(err(&expanding, "h", o.clone()).contains("MAX_ENCODED_FILENAME_BYTES"));
        let mut ttl_low = o.clone();
        ttl_low.ttl_ms = Some(59_999);
        assert!(err("k", "h", ttl_low).contains("out of range"));
        let mut ttl_high = o.clone();
        ttl_high.ttl_ms = Some(86_400_001);
        assert!(err("k", "h", ttl_high).contains("out of range"));
        // No filesystem writes happened.
        assert!(!td.path().join(".fno/claims").exists());
    }

    // ---- YAML read/write parity (contract item 2) -------------------------

    #[test]
    fn pid_claim_omits_expires_at_entirely() {
        let td = TempDir::new().unwrap();
        let out = acquire("session:u1", "pty:aa", opts_in(&td));
        assert!(matches!(out, AcquireOutcome::Acquired(_)));
        let text = std::fs::read_to_string(lockfile(&td, "session:u1")).unwrap();
        // Absent-not-null discipline: no expires_at LINE at all.
        assert!(
            !text.contains("expires_at"),
            "PID claim must omit expires_at: {text}"
        );
        assert!(text.contains("schema_version: 1"));
    }

    #[test]
    fn ttl_claim_serializes_integer_expires_at() {
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(60_000);
        let rec = match acquire("session:u2", "pty:bb", o) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rec.expires_at, Some(rec.acquired_at + 60_000));
        let text = std::fs::read_to_string(lockfile(&td, "session:u2")).unwrap();
        assert!(text.contains(&format!("expires_at: {}", rec.expires_at.unwrap())));
    }

    #[test]
    fn reader_treats_null_and_absent_expires_at_the_same() {
        let rec = parse_claim_str(
            "schema_version: 1\nkey: k\nholder: h\nacquired_at: 5\npid: 1\nhost: x\nexpires_at: null\n",
        )
        .unwrap_or_else(|_| panic!("null expires_at must parse"));
        assert_eq!(rec.expires_at, None);
    }

    #[test]
    fn reader_ignores_unknown_fields_and_defaults_schema_version() {
        let rec = parse_claim_str(
            "key: k\nholder: h\nacquired_at: 5\npid: 1\nhost: x\nfuture_field: [1, 2]\n",
        )
        .expect("unknown fields must be ignored");
        assert_eq!(rec.schema_version, 1);
        assert!(rec.metadata.is_empty());
    }

    #[test]
    fn reader_rejects_newer_schema_non_dict_and_garbage_as_corrupted() {
        for text in [
            "schema_version: 2\nkey: k\nholder: h\nacquired_at: 5\npid: 1\nhost: x\n",
            "- just\n- a\n- list\n",
            "{{{{not yaml",
            "key: ''\nholder: h\nacquired_at: 5\npid: 1\nhost: x\n",
        ] {
            assert!(
                matches!(parse_claim_str(text), Err(ReadError::Corrupted(_))),
                "should be corrupted: {text}"
            );
        }
    }

    #[test]
    fn metadata_survives_yaml_roundtrip() {
        let mut meta = Map::new();
        meta.insert("nested".into(), json!({"a": [1, 2], "b": "text"}));
        meta.insert("flag".into(), json!(true));
        let rec = ClaimRecord {
            schema_version: 1,
            key: "session:u".into(),
            holder: "h".into(),
            acquired_at: 42,
            pid: 7,
            host: "hh".into(),
            expires_at: None,
            reason: Some("why".into()),
            metadata: meta,
        };
        let text = serialize_claim(&rec).unwrap();
        let back = parse_claim_str(&text).unwrap();
        assert_eq!(back, rec);
    }

    // ---- liveness classification (contract item 8) ------------------------

    fn record(pid: i32, acquired_at: i64, expires_at: Option<i64>, host: &str) -> ClaimRecord {
        ClaimRecord {
            schema_version: 1,
            key: "session:x".into(),
            holder: "h".into(),
            acquired_at,
            pid,
            host: host.into(),
            expires_at,
            reason: None,
            metadata: Map::new(),
        }
    }

    #[test]
    fn liveness_matches_python_classify_including_hybrid_arm() {
        let me = std::process::id() as i32;
        let host = hostname();
        let now = now_ms();
        // PID claim, our own live pid, acquired now -> LIVE.
        assert_eq!(
            classify(&record(me, now, None, &host), Some(now)),
            ClaimState::Live
        );
        // PID-reuse: acquired_at BEFORE our process started -> STALE.
        assert_eq!(
            classify(&record(me, 1, None, &host), Some(now)),
            ClaimState::Stale
        );
        // Cross-host is never live.
        assert_eq!(
            classify(&record(me, now, None, "elsewhere.example"), Some(now)),
            ClaimState::Stale
        );
        // Unexpired TTL + LIVE pid -> LIVE.
        assert_eq!(
            classify(&record(me, now, Some(now + 60_000), &host), Some(now)),
            ClaimState::Live
        );
        // SUSPECT arm (x-ba4b): unexpired TTL + dead/replaced pid -> SUSPECT
        // (was LIVE). A respawned worker's slot stays TTL-protected, but the
        // distinct state lets init/dispatch refuse-and-skip rather than steal.
        assert_eq!(
            classify(&record(-1, now, Some(now + 60_000), &host), Some(now)),
            ClaimState::Suspect
        );
        // SUSPECT is off-host too: unexpired TTL but a foreign host pid.
        assert_eq!(
            classify(&record(me, now, Some(now + 60_000), "elsewhere.example"), Some(now)),
            ClaimState::Suspect
        );
        // HYBRID arm: expired TTL + live recorded pid -> LIVE.
        assert_eq!(
            classify(&record(me, now, Some(now - 1), &host), Some(now)),
            ClaimState::Live
        );
        // Expired TTL + dead pid -> STALE.
        assert_eq!(
            classify(&record(-1, now, Some(now - 1), &host), Some(now)),
            ClaimState::Stale
        );
    }

    #[test]
    fn own_process_create_time_is_sane() {
        let create = process_create_time_ms(std::process::id() as i32)
            .expect("must be able to inspect our own pid");
        let now = now_ms();
        assert!(create <= now, "create {create} must not postdate now {now}");
        // Started within the last day (a directional sanity bound).
        assert!(now - create < 86_400_000);
        // A pid that cannot exist reads as dead.
        assert_eq!(process_create_time_ms(-1), None);
    }

    // ---- acquire / release / status semantics (contract items 3-7) --------

    #[test]
    fn fresh_acquire_writes_lockfile_and_emits() {
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.reason = Some("testing".into());
        let rec = match acquire("session:fresh", "pty:me", o) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rec.pid, std::process::id() as i32);
        assert!(lockfile(&td, "session:fresh").exists());
        let events = read_events(&td);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "claim_acquired");
        assert_eq!(events[0]["source"], "abi-loop");
        assert_eq!(events[0]["data"]["holder"], "pty:me");
        assert_eq!(events[0]["data"]["reason"], "testing");
        assert_eq!(events[0]["data"]["expires_at"], Value::Null);
    }

    #[test]
    fn same_holder_reacquire_is_idempotent_and_refreshes() {
        let td = TempDir::new().unwrap();
        let first = match acquire("session:idem", "pty:me", opts_in(&td)) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        let mut o = opts_in(&td);
        o.pid = Some(4242);
        let second = match acquire("session:idem", "pty:me", o) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(second.pid, 4242);
        assert!(second.acquired_at >= first.acquired_at);
        let events = read_events(&td);
        assert_eq!(events[1]["type"], "claim_idempotent_reacquired");
        assert_eq!(events[1]["data"]["previous_acquired_at"], first.acquired_at);
    }

    #[test]
    fn live_other_holder_is_refused_with_identity() {
        let td = TempDir::new().unwrap();
        assert!(matches!(
            acquire("session:held", "pty:owner", opts_in(&td)),
            AcquireOutcome::Acquired(_)
        ));
        match acquire("session:held", "pty:intruder", opts_in(&td)) {
            AcquireOutcome::HeldByOther { holder, pid, .. } => {
                assert_eq!(holder, "pty:owner");
                assert_eq!(pid, std::process::id() as i32);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stale_claim_is_reclaimed_archived_and_audited() {
        let td = TempDir::new().unwrap();
        // A claim whose acquired_at predates this process's create time reads
        // as PID reuse -> stale.
        let mut o = opts_in(&td);
        o.pid = Some(std::process::id());
        let stale = record(std::process::id() as i32, 1, None, &hostname());
        let path = lockfile(&td, "session:x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serialize_claim(&stale).unwrap()).unwrap();

        let rec = match acquire("session:x", "pty:new", o) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rec.holder, "pty:new");
        // Forensic trail: archived by rename, never unlinked.
        let expired: Vec<_> = std::fs::read_dir(path.parent().unwrap().join(EXPIRED_SUBDIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(expired.len(), 1);
        assert!(expired[0].starts_with("session%3Ax."));
        let events = read_events(&td);
        assert_eq!(events.last().unwrap()["type"], "claim_stale_reclaimed");
        assert_eq!(events.last().unwrap()["data"]["previous_holder"], "h");
    }

    #[test]
    fn corrupted_file_status_reports_acquire_refuses_release_leaves() {
        let td = TempDir::new().unwrap();
        let path = lockfile(&td, "session:bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{{{{not yaml").unwrap();

        let (state, rec) = status("session:bad", Some(td.path()));
        assert_eq!(state, ClaimState::Corrupted);
        assert!(rec.is_none());
        assert!(matches!(
            acquire("session:bad", "pty:x", opts_in(&td)),
            AcquireOutcome::Error(_)
        ));
        // Non-strict release: silent success, file LEFT for force-release.
        release("session:bad", "pty:x", Some(td.path()), Some(td.path())).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn release_semantics_missing_other_holder_and_owned() {
        let td = TempDir::new().unwrap();
        // Missing file: silent success.
        release("session:gone", "pty:x", Some(td.path()), Some(td.path())).unwrap();
        // Different holder: silent no-op, file kept.
        assert!(matches!(
            acquire("session:r", "pty:owner", opts_in(&td)),
            AcquireOutcome::Acquired(_)
        ));
        release("session:r", "pty:other", Some(td.path()), Some(td.path())).unwrap();
        assert!(lockfile(&td, "session:r").exists());
        // Our own: unlinked + audited with duration.
        release("session:r", "pty:owner", Some(td.path()), Some(td.path())).unwrap();
        assert!(!lockfile(&td, "session:r").exists());
        let events = read_events(&td);
        let released = events.last().unwrap();
        assert_eq!(released["type"], "claim_released");
        assert!(released["data"]["duration_held_ms"].as_i64().unwrap() >= 0);
    }

    #[test]
    fn status_reads_free_live_and_full_record() {
        let td = TempDir::new().unwrap();
        assert_eq!(
            status("session:s", Some(td.path())),
            (ClaimState::Free, None)
        );
        let mut o = opts_in(&td);
        let mut meta = Map::new();
        meta.insert("k".into(), json!("v"));
        o.metadata = Some(meta.clone());
        acquire("session:s", "pty:me", o);
        let (state, rec) = status("session:s", Some(td.path()));
        assert_eq!(state, ClaimState::Live);
        let rec = rec.unwrap();
        assert_eq!(rec.holder, "pty:me");
        assert_eq!(rec.metadata, meta);
    }

    // ---- recovery mutex (contract item 6) ---------------------------------

    #[test]
    fn held_recovery_mutex_is_waited_on_then_recovery_proceeds() {
        let td = TempDir::new().unwrap();
        let path = lockfile(&td, "session:x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stale = record(std::process::id() as i32, 1, None, &hostname());
        std::fs::write(&path, serialize_claim(&stale).unwrap()).unwrap();
        // Simulate a peer (Python or Rust) mid-recovery, releasing shortly.
        let mutex = path.with_file_name(format!(
            "{}.recovery.d",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir(&mutex).unwrap();
        let mutex_clone = mutex.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            std::fs::remove_dir(&mutex_clone).unwrap();
        });
        let out = acquire("session:x", "pty:waiter", opts_in(&td));
        releaser.join().unwrap();
        assert!(matches!(out, AcquireOutcome::Acquired(_)), "{out:?}");
    }

    #[test]
    fn deadline_expired_waiter_never_steals_the_recovery_mutex() {
        let td = TempDir::new().unwrap();
        let path = lockfile(&td, "session:x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stale = record(std::process::id() as i32, 1, None, &hostname());
        std::fs::write(&path, serialize_claim(&stale).unwrap()).unwrap();
        let mutex = path.with_file_name(format!(
            "{}.recovery.d",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir(&mutex).unwrap();
        // Held for the whole call: acquire retries, exhausts attempts, errors.
        // The mutex must survive (stealing would reintroduce the TOCTOU
        // double-winner) and the stale claim must be untouched.
        wait_for_recovery_release(&mutex, Duration::from_millis(50)); // exercise the wait path cheaply
        let out = recover_stale(&path, "session:x", "pty:thief", &opts_in(&td), None);
        assert!(matches!(out, RecoverResult::Retry));
        assert!(mutex.exists(), "recovery mutex was stolen");
        let kept = read_claim_file(&path).ok().unwrap();
        assert_eq!(kept.holder, "h");
    }

    #[test]
    fn simultaneous_acquire_has_exactly_one_winner() {
        let td = TempDir::new().unwrap();
        let root = td.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let o = AcquireOpts {
                        root: Some(root.clone()),
                        events_dir: Some(root),
                        ..Default::default()
                    };
                    acquire("session:race", &format!("pty:w{i}"), o)
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = outcomes
            .iter()
            .filter(|o| matches!(o, AcquireOutcome::Acquired(_)))
            .count();
        assert_eq!(winners, 1, "{outcomes:?}");
        // Losers saw the winner's identity, not an error.
        assert!(
            outcomes
                .iter()
                .all(|o| !matches!(o, AcquireOutcome::Error(_))),
            "{outcomes:?}"
        );
        // The surviving lockfile parses cleanly.
        assert!(read_claim_file(&lockfile(&td, "session:race")).is_ok());
    }
}
