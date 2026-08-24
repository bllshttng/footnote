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
//!   mutex; a waiter that times out retries acquire, and NEVER rmdirs a mutex
//!   in place. A mutex older than `STALE_MUTEX_STEAL` is a corpse and is taken
//!   by atomic rename (exactly one stealer wins) so a killed recoverer cannot
//!   brick a claim key forever;
//! - stale claims are archived by rename to `.expired/<enc>.<now_ms>.lock`,
//!   never unlinked;
//! - hybrid liveness, corroborated: an expired-TTL claim whose recorded pid is
//!   a live process on this machine is still LIVE only when the pid was
//!   prover-proven at write time (`pid_provenance == "session-prover"`); a
//!   live pid under any other provenance falls to STALE, so a foreign process
//!   can never make a lease permanent. PID-reuse is detected by comparing
//!   the process create time (epoch ms) against `acquired_at`;
//! - liveness compares the additive `machine_id` field (`hostid.machine_id`),
//!   NOT `host`/`gethostname(2)`; both implementations must write and compare
//!   it identically or each reads the other's claims as cross-machine.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// On-disk schema version this implementation reads and writes. Readers
/// refuse anything newer rather than guess at a future writer's semantics.
pub const SCHEMA_VERSION: u32 = 1;
/// Version used only for the incompatible nullable-PID claim shape. Version 1
/// remains the writer format for integer-PID claims and remains readable.
pub const PID_UNAVAILABLE_SCHEMA_VERSION: u32 = 2;
pub const MAX_SUPPORTED_SCHEMA_VERSION: u32 = PID_UNAVAILABLE_SCHEMA_VERSION;
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
    pub pid: Option<i32>,
    pub host: String,
    /// True only for TTL claims whose durable PID could not be proven.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pid_unavailable: bool,
    /// Epoch ms of TTL expiry; absent (and treated same as null on read) for
    /// PID-liveness claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Owning harness (`codex`/`claude`/`gemini`), resolved from the acquiring
    /// process's ambient session markers. Additive: absent on pre-change records
    /// (reads as `None` == unknown, never a parse error) and omitted when no
    /// marker is present. The legible primitive the dispatch guard reads to tell
    /// a foreign-harness owner from a native one without parsing the holder id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// How `pid` was resolved at write time (mirrors `types.Claim.pid_provenance`):
    /// "session-prover" = provably the acquiring session's own process (the
    /// process-tree prover's answer, or the claimant itself when the pid
    /// defaults to this process); "ambient" = caller-supplied, unverifiable
    /// here. The expired-TTL hybrid arm in `classify` reads it: only a
    /// prover-proven pid keeps an expired claim Live, so a long-lived foreign
    /// process can never make a lease permanent. Additive: absent on
    /// pre-change records (reads as `None` == ambient).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_provenance: Option<String>,
    /// Stable machine identity (mirrors `fno.claims.hostid.machine_id`).
    /// Additive for the same reason `harness` is: absent on pre-change records
    /// (reads as `None`, never a parse error). Overwriting `host` instead would
    /// make a still-running pre-change reader compare a machine id against its
    /// `gethostname(2)`, miss, and call a LIVE claim stale, which is stealable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// Opaque; preserved byte-for-byte through idempotent re-acquires.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Options for [`acquire`]. `pid` defaults to the calling process — which,
/// natively, is the long-lived daemon/worker rather than a transient CLI
/// subprocess, so the claim is live from birth (closes the acquire-to-reanchor
/// stale window the shelled implementation had).
#[derive(Debug, Default, Clone)]
pub struct AcquireOpts {
    pub pid: Option<u32>,
    pub pid_unavailable: bool,
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
        pid: Option<i32>,
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

/// The claims DIRECTORY (`<root>/.fno/claims`) for an explicit root, else the
/// global root. `None` when no root resolves (no `$FNO_CLAIMS_ROOT`, no
/// `$HOME`) — callers sweep-read fail-open on that.
pub(crate) fn claims_dir_for(root: Option<&Path>) -> Option<PathBuf> {
    match root {
        Some(r) => Some(r.join(CLAIMS_DIRNAME)),
        None => global_claims_root().map(|r| r.join(CLAIMS_DIRNAME)),
    }
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

/// macOS IOPlatformUUID: per-machine, survives renames and roaming.
#[cfg(target_os = "macos")]
fn platform_machine_id() -> String {
    let out = match std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return String::new(),
    };
    // `    "IOPlatformUUID" = "0A1B..."` — split rather than pull in a regex dep.
    out.split_once("\"IOPlatformUUID\" = \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(id, _)| id.to_string())
        .unwrap_or_default()
}

/// Linux: systemd writes `/etc/machine-id`; dbus the second path.
#[cfg(target_os = "linux")]
fn platform_machine_id() -> String {
    let mut base = String::new();
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let value = text.trim();
            if !value.is_empty() {
                base = value.to_string();
                break;
            }
        }
    }
    if base.is_empty() {
        return base;
    }
    // Containers from one image share /etc/machine-id but hold INDEPENDENT pid
    // namespaces; without the namespace two of them sharing a claims root read
    // each other's pids as local, so a dead foreign claim classifies LIVE
    // forever instead of staying opaque.
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata("/proc/self/ns/pid") {
        Ok(md) => format!("{base}:{}", md.ino()),
        Err(_) => base,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_machine_id() -> String {
    String::new()
}

/// A stable identifier for this machine, or "" when there is none (mirrors
/// `fno.claims.hostid.machine_id`). Never substitutes `hostname()`: readers
/// treat a present value as authoritative.
///
/// `gethostname(2)` is NOT a stable machine identity: on macOS with
/// `scutil --get HostName` unset it is derived from whatever DHCP/DNS last
/// supplied and flips on network join, VPN, and sleep/wake. The claim `host`
/// field scopes PID-reuse detection, so keying it on a moving string made a
/// live holder read cross-host — short-circuiting `is_live` before the pid
/// check — and drop to Stale, which is stealable.
///
/// Cached: the macOS arm shells out to `ioreg`, and a sweep reads many
/// lockfiles against one machine identity.
fn machine_id() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(platform_machine_id).clone()
}

/// Was this claim written on THIS machine? (mirrors
/// `fno.claims.hostid.is_same_machine`)
///
/// `machine` is authoritative whenever present. It is absent only on a claim
/// written before the field existed; those reproduce the old hostname compare
/// exactly, so a pre-change claim classifies no worse than it does today.
fn is_same_machine(host: &str, machine: Option<&str>) -> bool {
    // The machine arm decides only when BOTH sides have an id. A reader that
    // cannot read its own is "unknown", not "a different machine": answering
    // false there would stale a live local claim and make it stealable, and
    // ambiguous liveness must degrade to skip, never to steal.
    let mine = machine_id();
    if let Some(m) = machine.filter(|m| !m.is_empty()) {
        if !mine.is_empty() {
            return m == mine;
        }
        // Claim names a machine but our own id is unreadable: UNKNOWN, not
        // foreign. Falling through to the hostname compare would stale a live
        // local claim whenever the name has also moved. The pid arm still
        // decides, so this only ever withholds a steal.
        return true;
    }
    if host.is_empty() {
        return false;
    }
    host == hostname()
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
/// False when: cross-machine, pid gone/uninspectable, or the current occupant
/// of the pid slot started AFTER the claim was filed (PID reuse).
fn is_live(rec: &ClaimRecord) -> bool {
    if rec.pid_unavailable {
        return false;
    }
    if !is_same_machine(&rec.host, rec.machine_id.as_deref()) {
        return false;
    }
    match rec.pid.and_then(process_create_time_ms) {
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
/// INCLUDING the corroborated hybrid arm: an expired-TTL claim whose recorded
/// pid is a live process on this host is still LIVE only when that pid was
/// prover-proven at write time - a suspended-but-alive session must not have
/// its claim reclaimed by a peer, while a live FOREIGN pid (a chat app's
/// app-server answering for the holder) must not make the lease permanent).
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
        // Corroborated hybrid: the pid keeps the claim Live only when it was
        // proven to be the holder session's own process. Any other provenance
        // (or a legacy record with no field) is Stale, as a pre-hybrid claim
        // was: the TTL is a lease.
        let corroborated = rec.pid_provenance.as_deref() == Some("session-prover") && is_live(rec);
        return if corroborated {
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
pub(crate) enum ReadError {
    /// File disappeared between decision and read.
    GoneAway,
    /// Unparseable YAML, non-mapping root, schema violation, or io error.
    Corrupted(String),
}

fn serialize_claim(rec: &ClaimRecord) -> Result<String, String> {
    validate_record(rec).map_err(|e| format!("claim YAML serialize failed: {e}"))?;
    serde_yaml_ng::to_string(rec).map_err(|e| format!("claim YAML serialize failed: {e}"))
}

fn validate_record(rec: &ClaimRecord) -> Result<(), String> {
    if rec.key.is_empty() || rec.holder.is_empty() {
        return Err("claim key/holder must be non-empty".into());
    }
    if rec.pid_unavailable && rec.schema_version != PID_UNAVAILABLE_SCHEMA_VERSION {
        return Err("pid_unavailable claims require schema_version=2".into());
    }
    if !rec.pid_unavailable && rec.schema_version == PID_UNAVAILABLE_SCHEMA_VERSION {
        return Err("schema_version=2 requires pid_unavailable: true".into());
    }
    match (rec.pid, rec.pid_unavailable, rec.expires_at) {
        (Some(pid), false, _) if pid > 0 => Ok(()),
        (None, true, Some(_)) => Ok(()),
        (Some(_), true, _) => Err("pid and pid_unavailable are mutually exclusive".into()),
        (None, true, None) => Err("pid_unavailable requires a TTL claim".into()),
        (Some(_), false, _) => Err("claim pid must be positive".into()),
        (None, false, _) => Err("claim requires a positive pid or pid_unavailable: true".into()),
    }
}

fn parse_claim_str(text: &str) -> Result<ClaimRecord, ReadError> {
    let rec: ClaimRecord = serde_yaml_ng::from_str(text)
        .map_err(|e| ReadError::Corrupted(format!("claim parse/schema failed: {e}")))?;
    if rec.schema_version > MAX_SUPPORTED_SCHEMA_VERSION {
        return Err(ReadError::Corrupted(format!(
            "claim schema_version={} > supported={MAX_SUPPORTED_SCHEMA_VERSION}; refusing to read from a newer writer",
            rec.schema_version
        )));
    }
    validate_record(&rec).map_err(ReadError::Corrupted)?;
    Ok(rec)
}

pub(crate) fn read_claim_file(path: &Path) -> Result<ClaimRecord, ReadError> {
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
/// SAME envelope the Python emitter writes: `{ts, type, source: "fno-loop",
/// data}` — so an operator reading the log (or `fno doctor event audit`) sees the
/// identical record regardless of which implementation performed the
/// operation. Deliberately NOT the crate's Branch-B `EventEmitter`: that
/// envelope is kind-flat with a 500-byte payload cap, either of which would
/// break record parity for these events.
///
/// Serializes on the cross-language `events.jsonl.lock.d` mkdir mutex shared
/// with `fno.events.append_event` and the loop Journal. The shell writers do
/// not take this lock and instead cap their fixed-shape serialized lines below
/// the atomic append bound. This path uses a short bounded wait because it runs
/// on daemon hot paths; a wedged lock logs and skips rather than blocking. The
/// lockfile write is authoritative; this log is observability only.
fn emit_claim_event(events_dir: Option<&Path>, type_name: &str, data: Map<String, Value>) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let events_path = claim_events_path(events_dir, &cwd);
    let event = json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "type": type_name,
        "source": "fno-loop",
        "data": Value::Object(data),
    });
    if let Err(e) = append_event_line(&events_path, &event, Duration::from_secs(2)) {
        eprintln!("claims: failed to emit {type_name:?}: {e}");
    }
}

/// The journal a claim audit event lands in.
///
/// Precedence mirrors `fno.paths.project_events_json` and `scripts/lib/events.sh`
/// exactly: an explicitly named journal wins, then the `FNO_EVENTS_PATH` pin,
/// then the repo root resolved from `cwd`.
///
/// Reading the pin here is not tidiness. Python and Rust share this journal AND
/// its `.lock.d` mutex as a wire contract, so a pin only one side honors puts
/// the two writers on different files and their mutex stops serialising them
/// against each other. `cli/tests/integration/test_claims_cross_impl.py` is the
/// merge gate that catches it: Rust acquires, Python reclaims, and the audit
/// trail has to be one file.
fn claim_events_path(events_dir: Option<&Path>, cwd: &Path) -> PathBuf {
    let pin = std::env::var("FNO_EVENTS_PATH").ok();
    claim_events_path_with(events_dir, cwd, pin.as_deref())
}

/// The pure core of [`claim_events_path`], taking the pin as an argument.
///
/// Split out so a test can pin the precedence without mutating process env:
/// Rust tests share one process and run threaded, so `set_var`/`remove_var`
/// would race every other test in the binary.
fn claim_events_path_with(events_dir: Option<&Path>, cwd: &Path, pin: Option<&str>) -> PathBuf {
    if let Some(dir) = events_dir {
        return dir.join(".fno/events.jsonl");
    }
    // Empty means unset, matching `if override:` in Python and `-n` in the
    // shell. Deliberately NOT trimmed: neither of those trims, and three
    // writers disagreeing about a whitespace pin is worse than all three
    // treating it as a path.
    if let Some(pinned) = pin.filter(|p| !p.is_empty()) {
        return PathBuf::from(pinned);
    }
    crate::paths::worktree_repo_root(cwd).join(".fno/events.jsonl")
}

/// Age past which a mkdir mutex dir is a corpse left by a killed holder.
///
/// Mirrors `fno.mutex.STALE_MUTEX_STEAL_S`. The `.recovery.d` mutex is wire
/// protocol with the Python implementation, so the threshold and the steal rule
/// must move together. Every critical section under these mutexes is
/// sub-second; never do slow work (network, subprocess) inside one or the age
/// predicate stops distinguishing a corpse from an honest holder.
const STALE_MUTEX_STEAL: Duration = Duration::from_secs(120);

/// A dir a steal disturbed and then put back (owner-token mismatch: a live
/// lock, not the corpse we aged) is granted only this much fresh protection,
/// not a full [`STALE_MUTEX_STEAL`]. Mirrors `fno.mutex.RESTORE_GRACE_S`; see
/// its docstring for why - a disturbed holder is either about to release
/// within milliseconds, or already gone, and only the second case is hurt by
/// the shortened window. Wire protocol with the Python side; change both in
/// lockstep, see `test_threshold_matches_the_rust_constant`.
const RESTORE_GRACE: Duration = Duration::from_millis(500);

/// Rename-steal `lock_dir` when it is older than [`STALE_MUTEX_STEAL`].
///
/// True means retry `create_dir` immediately (the corpse is gone, or the lock
/// was already released); false means the lock is honestly held and the caller
/// should wait exactly as before. Removal happens only via an atomic rename the
/// remover won, so two stealers can never both clear the same corpse.
fn steal_if_stale(lock_dir: &Path) -> bool {
    // symlink_metadata, not metadata: a dangling symlink at the lock path is
    // stattable only without following it, and `create_dir` reports it as
    // AlreadyExists. Following would yield NotFound here, and a caller that
    // retries on true would spin against a lock it can never acquire.
    let before = match std::fs::symlink_metadata(lock_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false, // unstattable for any other reason: wait, never spin
    };
    // A clock that runs backwards yields Err here -> zero age -> no steal.
    let age = before
        .modified()
        .map(|t| t.elapsed().unwrap_or_default())
        .unwrap_or_default();
    if age <= STALE_MUTEX_STEAL {
        return false;
    }

    // Capture the corpse's identity BEFORE the rename: between this and the
    // rename another stealer can win and a fresh holder acquire at the same
    // path, so what we move may be a LIVE lock. The owner token is the identity
    // check (inode recycling fooled the old inode+mtime compare).
    let before_token = read_owner(lock_dir);
    let before_modified = before.modified().ok();
    // Unique per attempt (see the Python twin): one name per pid means a reap
    // dir left by a failed cleanup collides forever, silently disabling every
    // future steal by this process.
    static REAP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let reaped = lock_dir.with_file_name(format!(
        "{}.reap.{}.{}",
        lock_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
        REAP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    match std::fs::rename(lock_dir, &reaped) {
        Ok(()) => {
            // A live lock swapped in carries a different owner token; a holder
            // that renewed after the age read keeps its token but changes the
            // directory mtime. Either signal means put it back and lose.
            if !same_stale_lease(&reaped, &before_token, before_modified) {
                // A restored dir was disturbed once already: its holder may
                // have released into the gap, leaving a lock nobody will
                // remove. A fresh mtime would shield that orphan for the full
                // steal threshold, so hand back only an honest-hold grace
                // window instead - a live holder releases inside it; an
                // orphan becomes stealable in RESTORE_GRACE rather than
                // STALE_MUTEX_STEAL (mirrors the Python fix, x-474a).
                backdate_mtime(&reaped, STALE_MUTEX_STEAL - RESTORE_GRACE);
                if std::fs::rename(&reaped, lock_dir).is_err() {
                    eprintln!(
                        "claims: stole a live mutex at {} and could not restore it",
                        lock_dir.display()
                    );
                }
                return false;
            }
            eprintln!(
                "claims: stole stale mutex {} (age {}s)",
                lock_dir.display(),
                age.as_secs()
            );
            remove_reaped(&reaped);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            eprintln!(
                "claims: could not steal stale mutex {}: {e}",
                lock_dir.display()
            );
            false
        }
    }
}

/// Unique-per-acquire ownership token: `host:pid:ns`. Mirrors
/// `fno.mutex._owner_token`; stamped into `lock_dir/owner` so a release can
/// verify it owns the lock before removing the dir (a stealer that renamed the
/// dir out from under a holder mid-write must not let the holder's trailing
/// remove delete the new holder's live lock).
fn owner_token() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}:{}:{}", hostname(), std::process::id(), ns)
}

fn read_owner(lock_dir: &Path) -> String {
    std::fs::read_to_string(lock_dir.join("owner")).unwrap_or_default()
}

/// Generate a token, write it to `lock_dir/owner`, return it. Mirrors
/// `fno.mutex._stamp_owner`. Called right after the mkdir acquire; the
/// mkdir-to-stamp gap is safe by the age gate, and a crash here leaves a
/// no-owner corpse the age gate steals exactly as before.
fn stamp_owner(lock_dir: &Path) -> String {
    let token = owner_token();
    let _ = std::fs::write(lock_dir.join("owner"), &token);
    token
}

/// Acquire a mkdir dir mutex; return an owner token, or None on timeout.
/// Mirrors `fno.mutex.acquire_dir_mutex`. None means a live, in-age holder was
/// held past the deadline - genuine congestion, not a corpse.
fn acquire_dir_mutex(lock_dir: &Path, timeout: Duration, steal: bool) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::create_dir(lock_dir) {
            Ok(()) => return Some(stamp_owner(lock_dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if steal && steal_if_stale(lock_dir) {
                    continue;
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Remove `lock_dir` only when its owner token matches; never raise. Mirrors
/// `fno.mutex.release_dir_mutex`. A mismatch (or missing owner file) means the
/// lock was stolen or replaced mid-write: leave the current holder's dir intact.
/// The dir contains an `owner` file, so removal is `remove_dir_all`.
fn release_dir_mutex(lock_dir: &Path, token: &str) {
    if read_owner(lock_dir) == token {
        let _ = std::fs::remove_dir_all(lock_dir);
        return;
    }
    eprintln!(
        "claims: release_dir_mutex {} no longer owned by {}; left intact",
        lock_dir.display(),
        token
    );
}

/// Set `path`'s mtime to `age` in the past. Best-effort: a failure (read-only
/// mount, path vanished mid-restore) is swallowed, mirroring
/// `fno.mutex.steal_if_stale`'s `os.utime` - a reporting-adjacent backdate must
/// never fail the restore it precedes. Via libc directly (already a direct
/// dependency, see the test-only `age_dir` twin below) rather than pulling in
/// a crate for one syscall.
fn backdate_mtime(path: &Path, age: Duration) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let backdated = now.saturating_sub(age);
    // Sub-second precision matters here: truncating to tv_usec: 0 rounds the
    // timestamp DOWN (older), and RESTORE_GRACE is itself sub-second (500ms),
    // so dropping up to 999ms could already push the restored dir past
    // STALE_MUTEX_STEAL the instant it lands - a zero-length grace window.
    let t = libc::timeval {
        tv_sec: backdated.as_secs() as libc::time_t,
        tv_usec: backdated.subsec_micros() as libc::suseconds_t,
    };
    let times = [t, t];
    unsafe {
        libc::utimes(c.as_ptr(), times.as_ptr());
    }
}

/// Identity for a reaped lock dir via owner token. Mirrors
/// `fno.mutex._same_owner`: a token match, or an empty owner file (a pre-token
/// corpse from a crashed acquirer or an old binary), means we reaped what we
/// aged; a different token means a live lock was swapped in.
fn same_owner(path: &Path, before_token: &str) -> bool {
    let after = read_owner(path);
    after.is_empty() || after == before_token
}

fn same_stale_lease(path: &Path, before_token: &str, before_modified: Option<SystemTime>) -> bool {
    same_owner(path, before_token)
        && std::fs::symlink_metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            == before_modified
}

/// Delete a reaped mutex, usually a directory but possibly a symlink
/// (`remove_dir_all` fails on one).
fn remove_reaped(path: &Path) {
    if std::fs::remove_file(path).is_ok() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "claims: could not remove reaped mutex {}: {e}",
                path.display()
            );
        }
    }
}

pub(crate) fn append_event_line(
    events_path: &Path,
    event: &Value,
    lock_timeout: Duration,
) -> Result<(), String> {
    let mut line = serde_json::to_vec(event).map_err(|e| e.to_string())?;
    line.push(b'\n');
    loop {
        // Setup can replace a local journal with a canonical-journal symlink
        // while this writer waits on the old mutex. Re-resolve after acquiring
        // and retry whenever the leaf changed during that handoff.
        let resolved_path =
            std::fs::canonicalize(events_path).unwrap_or_else(|_| events_path.to_path_buf());
        if let Some(parent) = resolved_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let lock_dir = resolved_path.with_file_name(format!(
            "{}.lock.d",
            resolved_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "events.jsonl".into())
        ));
        let token = acquire_dir_mutex(&lock_dir, lock_timeout, true)
            .ok_or_else(|| format!("events.jsonl lock timeout: {}", lock_dir.display()))?;
        let current_path =
            std::fs::canonicalize(events_path).unwrap_or_else(|_| events_path.to_path_buf());
        if current_path != resolved_path {
            release_dir_mutex(&lock_dir, &token);
            continue;
        }
        let res = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&resolved_path)
            .and_then(|mut f| f.write_all(&line))
            .map_err(|e| e.to_string());
        release_dir_mutex(&lock_dir, &token);
        return res;
    }
}

fn event_maintenance_dir(events_path: &Path) -> PathBuf {
    let resolved_path =
        std::fs::canonicalize(events_path).unwrap_or_else(|_| events_path.to_path_buf());
    resolved_path.with_file_name(format!(
        "{}.gc.d",
        resolved_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "events.jsonl".into())
    ))
}

pub(crate) fn event_maintenance_active(events_path: &Path) -> bool {
    std::fs::symlink_metadata(event_maintenance_dir(events_path)).is_ok()
}

pub(crate) fn wait_for_event_maintenance(events_path: &Path) {
    let maintenance_dir = event_maintenance_dir(events_path);
    loop {
        if std::fs::symlink_metadata(&maintenance_dir).is_err() {
            return;
        }
        if let Some(token) = acquire_dir_mutex(&maintenance_dir, Duration::from_secs(2), true) {
            release_dir_mutex(&maintenance_dir, &token);
            return;
        }
    }
}

/// Shared data fields for claim events (mirrors `events._common`, including
/// the explicit `expires_at: null` for PID-liveness claims — the EVENT payload
/// carries null where the LOCKFILE omits the key; that asymmetry is Python's).
fn common_event_data(rec: &ClaimRecord) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".into(), Value::String(rec.key.clone()));
    m.insert("holder".into(), Value::String(rec.holder.clone()));
    m.insert(
        "pid".into(),
        rec.pid.map(Value::from).unwrap_or(Value::Null),
    );
    m.insert("pid_unavailable".into(), Value::Bool(rec.pid_unavailable));
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

fn validate_inputs(
    key: &str,
    holder: &str,
    ttl_ms: Option<i64>,
    pid: Option<u32>,
    pid_unavailable: bool,
) -> Result<(), String> {
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
    if pid_unavailable && ttl_ms.is_none() {
        return Err("pid_unavailable requires a TTL claim".into());
    }
    if pid_unavailable && pid.is_some() {
        return Err("pid and pid_unavailable are mutually exclusive".into());
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

/// Ambient harness session markers, highest precedence first. Mirrors
/// `cli/src/fno/harness_identity.py::HARNESS_SESSION_MARKERS` (x-efc7) so the
/// Rust writer tags a claim with the same harness the Python resolver would.
pub(crate) const HARNESS_SESSION_MARKERS: &[(&str, &str)] = &[
    ("CODEX_THREAD_ID", "codex"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_SESSION_ID", "codex"),
    ("GEMINI_SESSION_ID", "gemini"),
    ("OPENCODE_SESSION_ID", "opencode"),
];

/// Resolve the owning harness from the ambient process environment. `None` when
/// no marker is set (a bare shell / daemon) - the claim then reads as unknown,
/// never blocking dispatch on a missing tag.
pub fn resolve_harness() -> Option<String> {
    resolve_harness_from(|k| std::env::var(k).ok())
}

/// Testable core of [`resolve_harness`]: `get` supplies each marker's value so
/// the resolution contract is exercised without mutating process-global env.
/// A set-but-blank marker is UNSET (matches the Python `.strip()` check), so a
/// lower-precedence real marker still wins.
///
/// A single harness family present (the dominant case: one marker, or several
/// that agree) resolves to it. Two DISAGREEING families are ambiguous, and
/// precedence must not silently launder an inherited marker - a foreign
/// CODEX_THREAD_ID lingering in a claude child's env - into the claim's harness
/// tag. The Rust writer cannot prove which marker this process owns (that needs
/// a process-tree/transcript check the Python resolver does), so it records
/// `None` for the ambiguous case rather than guessing; the authoritative proven
/// harness is stamped on the manifest by the init hook via that resolver.
///
/// That resolver is `resolve_self_identity` in `cli/src/fno/claims/self_identity.py`,
/// which supplies a process-tree prover to `resolve_owned_identity`. The two writers agree on
/// direction and differ in reach: Python proves an inherited marker foreign and
/// stamps the real one, while this side only knows the families disagree. Both
/// refuse rather than launder, so a claim written here is never WRONG, only
/// sometimes untagged. Keep it that way - resolving by precedence here would
/// reintroduce the leak on the one path the Python gate cannot see.
pub fn resolve_harness_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut resolved: Option<&'static str> = None;
    for (marker, harness) in HARNESS_SESSION_MARKERS {
        if get(marker).map(|v| !v.trim().is_empty()).unwrap_or(false) {
            if let Some(prev) = resolved {
                if prev != *harness {
                    return None;
                }
            } else {
                resolved = Some(*harness);
            }
        }
    }
    resolved.map(|h| h.to_string())
}

/// The spawn-time parent edge (x-132c), the Rust mirror of Python's
/// `_capture_parent_edge` (dispatch.py): ambient-captured from the SPAWNING
/// session's environment at every registry mint site, never required of a
/// caller. Returns `(session, harness, cwd)` for the spawned row's
/// `spawned_by_*` fields.
///
/// Same family rules as [`resolve_harness_from`]: a set-but-blank marker is
/// unset, and two DISAGREEING families attribute NOTHING - a foreign inherited
/// marker must not be laundered into a parent record for the life of the row.
/// Within one family the earlier marker wins (`CODEX_THREAD_ID` over legacy
/// `CODEX_SESSION_ID`, matching Python's AC-EDGE-multi), and the winner's
/// VALUE is the parent session id, not just its harness kind.
pub fn ambient_parent_edge() -> (Option<String>, Option<String>, Option<String>) {
    ambient_parent_edge_from(|k| std::env::var(k).ok())
}

/// Testable core of [`ambient_parent_edge`]: `get` supplies each marker's
/// value so the precedence and refusal rules run without mutating env.
pub fn ambient_parent_edge_from(
    get: impl Fn(&str) -> Option<String>,
) -> (Option<String>, Option<String>, Option<String>) {
    let cwd = std::env::var_os("PWD")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty());
    let mut family: Option<&'static str> = None;
    let mut session: Option<String> = None;
    for (marker, harness) in HARNESS_SESSION_MARKERS {
        let Some(value) = get(marker)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        match family {
            None => {
                family = Some(harness);
                session = Some(value);
            }
            Some(prev) if prev != *harness => return (None, None, cwd),
            // Same family: the earlier (higher-precedence) marker keeps the id.
            Some(_) => {}
        }
    }
    (session, family.map(|h| h.to_string()), cwd)
}

fn make_claim(key: &str, holder: &str, opts: &AcquireOpts) -> ClaimRecord {
    let acquired = now_ms();
    let pid_unavailable = opts.pid_unavailable;
    ClaimRecord {
        schema_version: if pid_unavailable {
            PID_UNAVAILABLE_SCHEMA_VERSION
        } else {
            SCHEMA_VERSION
        },
        key: key.into(),
        holder: holder.into(),
        acquired_at: acquired,
        pid: if pid_unavailable {
            None
        } else {
            Some(opts.pid.unwrap_or_else(std::process::id) as i32)
        },
        host: hostname(),
        pid_unavailable,
        // Omitted, not backfilled with the hostname, when no stable id exists:
        // readers treat a present value as authoritative, so a substitute would
        // make two processes on one machine disagree and stale each other.
        machine_id: Some(machine_id()).filter(|m| !m.is_empty()),
        expires_at: opts.ttl_ms.map(|ttl| acquired + ttl),
        reason: opts.reason.clone(),
        harness: resolve_harness(),
        // A defaulted pid IS this process, so the claimant vouches for it
        // directly (the native daemon is long-lived, per AcquireOpts). An
        // explicitly passed pid is caller-supplied and unverifiable here, so
        // it stamps ambient and the corroborated hybrid arm will not extend
        // an expired lease on its say-so alone.
        pid_provenance: Some(
            if opts.pid.is_some() {
                "ambient"
            } else {
                "session-prover"
            }
            .to_string(),
        ),
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
    if let Err(e) = validate_inputs(key, holder, opts.ttl_ms, opts.pid, opts.pid_unavailable) {
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
                // `fno agents claim release --force`.
                return AcquireOutcome::Error(e);
            }
        };

        if existing.holder == holder {
            match idempotent_reacquire_guarded(&path, key, holder, &opts, events_dir.as_deref()) {
                RecoverResult::Done(outcome) => return outcome,
                RecoverResult::Retry => continue,
            }
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

/// Idempotent re-acquire under the shared `.recovery.d` mutex (mirrors
/// `core.acquire_claim`'s idempotent branch). Without this, a Rust worker's
/// unguarded write could race a Python `reap_dead_claims()` sweep: reap takes
/// this same mutex to re-verify a claim is still dead immediately before
/// archiving it, but nothing stopped a respawned worker from rewriting the
/// file as live in that exact window, so reap would archive the fresh write
/// instead of the dead claim it proved. Taking the mutex here closes that
/// gap the same way `recover_stale` already closes it for stale-reclaim.
///
/// Re-reads under the lock rather than trusting the caller's `existing`: a
/// different holder's stale-reclaim (or reap's archive) can complete and
/// release its own mutex cycle entirely in the gap between our caller's
/// unlocked read and this function's own (uncontended) mkdir(), so the file
/// on disk may no longer belong to `holder` by the time we get here.
/// Hand-rolls the same mkdir/AlreadyExists/steal-or-wait acquire as
/// [`acquire_dir_mutex`] (and [`recover_stale`] below, its pre-existing
/// sibling) rather than calling that shared helper: NOT an oversight.
/// acquire_dir_mutex's generic loop treats a NotFound error (parent dir
/// vanished) the same as any other transient error - sleep and retry
/// until the deadline - where this pattern instead fast-paths NotFound to
/// an immediate Retry with no wait (the claims dir itself is gone;
/// exclusive-create recreates it from the top). Routing through
/// acquire_dir_mutex as-is would silently turn that fast path into a real
/// wait. recover_stale hand-rolls the identical block for the same
/// reason; consolidating both onto one helper needs that helper to gain
/// the fast path first, verified against acquire_dir_mutex's other
/// callers (the events-log and maintenance-dir locks), not a 1-line swap.
fn idempotent_reacquire_guarded(
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
    let token = match std::fs::create_dir(&recovery_lock) {
        Ok(()) => stamp_owner(&recovery_lock),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !steal_if_stale(&recovery_lock) {
                wait_for_recovery_release(&recovery_lock, RECOVERY_LOCK_MAX_WAIT);
            }
            return RecoverResult::Retry;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RecoverResult::Retry,
        Err(e) => return RecoverResult::Done(AcquireOutcome::Error(e.to_string())),
    };

    let fresh_existing = match read_claim_file(path) {
        Ok(rec) => rec,
        Err(ReadError::GoneAway) => {
            // Vanished while we held the lock - let the top-level create win
            // the now-empty path.
            release_dir_mutex(&recovery_lock, &token);
            return RecoverResult::Retry;
        }
        Err(ReadError::Corrupted(e)) => {
            release_dir_mutex(&recovery_lock, &token);
            return RecoverResult::Done(AcquireOutcome::Error(e));
        }
    };
    if fresh_existing.holder != holder {
        // A different holder won the key between our caller's unlocked read
        // and this lock - re-classify from scratch instead of overwriting it.
        release_dir_mutex(&recovery_lock, &token);
        return RecoverResult::Retry;
    }

    let outcome = idempotent_reacquire(path, key, holder, opts, &fresh_existing, events_dir);
    release_dir_mutex(&recovery_lock, &token);
    RecoverResult::Done(outcome)
}

enum RecoverResult {
    Done(AcquireOutcome),
    /// Another worker holds (or held) the recovery mutex, or a third worker
    /// won a create race: retry the whole acquire.
    Retry,
}

/// Stale-claim recovery under the shared mkdir mutex. The mutex NAME
/// (`<lockfile-name>.recovery.d`) and the steal rule are wire protocol: they
/// are how a Python worker and this implementation serialize recovery of the
/// same claim, so both sides steal only past [`STALE_MUTEX_STEAL`] and only by
/// atomic rename. A mutex younger than that is never touched (the holder may
/// still be mid-archive); a waiter whose deadline expires just retries acquire.
///
/// Age-based steal is what keeps a killed recoverer from bricking a claim key
/// permanently: archive-by-rename and exclusive-create both arbitrate a winner
/// on their own, so the mutex is a spurious-retry guard, not the correctness
/// boundary.
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
    let token = match std::fs::create_dir(&recovery_lock) {
        Ok(()) => stamp_owner(&recovery_lock),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another worker is doing recovery -- or died holding the mutex.
            // Steal a corpse so a killed recoverer cannot brick this key
            // forever; otherwise wait briefly. Either way retry from the top:
            // the recovering worker either succeeded (we then see live-other)
            // or failed (we get another shot).
            if !steal_if_stale(&recovery_lock) {
                wait_for_recovery_release(&recovery_lock, RECOVERY_LOCK_MAX_WAIT);
            }
            return RecoverResult::Retry;
        }
        // The claims dir itself vanished (or another io failure): retry from
        // the top, where exclusive-create will recreate the parent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RecoverResult::Retry,
        Err(e) => return RecoverResult::Done(AcquireOutcome::Error(e.to_string())),
    };

    // Inside the mutex: release on ALL paths out.
    let result = recover_stale_locked(path, key, holder, opts, events_dir);
    release_dir_mutex(&recovery_lock, &token);
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
            data.insert(
                "previous_pid".into(),
                existing.pid.map(Value::from).unwrap_or(Value::Null),
            );
            emit_claim_event(events_dir, "claim_stale_reclaimed", data);
            RecoverResult::Done(AcquireOutcome::Acquired(new_claim))
        }
        Err(CreateError::AlreadyHeld) => RecoverResult::Retry,
        Err(CreateError::Io(e)) => RecoverResult::Done(AcquireOutcome::Error(e)),
    }
}

/// Poll for another worker's recovery mutex to clear (mirrors the polling
/// loop inside Python's `mutex.acquire_dir_mutex`): bounded wait, then the
/// caller retries acquire regardless. Only reached for a mutex young enough
/// to be honestly held; corpses are handled by [`steal_if_stale`] before this
/// is called.
fn wait_for_recovery_release(recovery_lock: &Path, max_wait: Duration) {
    // symlink_metadata, not exists(): a dangling symlink at the mutex path is
    // AlreadyExists to create_dir but absent to a following stat, so exists()
    // would report the lock free and burn every contention attempt instantly.
    let deadline = Instant::now() + max_wait;
    while std::fs::symlink_metadata(recovery_lock).is_ok() && Instant::now() < deadline {
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

/// Parse a human TTL string ("1h" / "30m" / "3600s" / bare digits) to
/// milliseconds. BARE digits are SECONDS (parity with the Python `_parse_ttl`
/// and the `sleep` convention), not milliseconds. Returns `None` on garbage or
/// a non-positive result, so the caller falls back to a default.
pub fn parse_ttl_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1_000)
    } else {
        (s, 1_000) // bare number = seconds
    };
    num.trim()
        .parse::<i64>()
        .ok()
        .map(|v| v.saturating_mul(mult))
        .filter(|v| *v > 0)
}

/// Best-effort lease renewal (x-ba4b): reset a live TTL claim's `expires_at` to
/// `now + ttl_ms`, but ONLY if the on-disk holder still matches `holder`.
/// `fno-agents loop-check` calls this on every stop with the manifest's own
/// TTL, so a respawned worker (whose supervisor pid died) keeps its claim fresh
/// under any pid with no separate heartbeat.
///
/// The deadline is `now + ttl_ms` (a FIXED window, never a growing span): using
/// the claim's `expires_at - acquired_at` would compound, leaving a dead
/// session's claim over-extended for hours. `holder`, `reason` and `metadata`
/// are always preserved.
///
/// RE-ANCHORING (x-05be). When the recorded pid is NOT live and the renewer is
/// on this machine, renewal rewrites `pid`, `host`, `machine_id` and
/// `acquired_at` together alongside `expires_at`. This function used to
/// preserve the pid, and that is what made SUSPECT mean two different things: a
/// respawned worker renewing under a new pid left a claim byte-identical to a
/// dead worker's — dead pid, unexpired TTL — so nothing on disk separated a
/// live session from a corpse, and every reader that must not steal from the
/// first was forced to protect the second.
///
/// PID-reuse detection survives BECAUSE the anchor moves WITH the pid, which is
/// the property the old comment was reaching for. Detection compares
/// `create_time(pid)` against `acquired_at`: the renewer started before it
/// renewed, so the rewritten claim reads live, and if that pid later dies and
/// the kernel recycles the number, the recycled process's create_time is after
/// the new `acquired_at` and reads reused exactly as today. Preserving a dead
/// pid while moving only `expires_at` is what broke the property, because it
/// kept a corpse anchored to a real acquire time forever.
///
/// The re-anchor is narrow on purpose. A LIVE recorded pid is never rewritten,
/// so a claim whose holder is running keeps its original anchor and a concurrent
/// writer under the same holder string cannot quietly take it over. Off-machine
/// claims are never rewritten either: we cannot read another box's pid table, so
/// a dead-looking pid there is unverifiable and only the TTL may move.
///
/// The whole mutate runs under the SAME per-claim recovery mutex `acquire` uses
/// for stale recovery, and re-reads inside the lock, so a renew can never clobber
/// a peer's concurrent stale-reclaim. An already-expired claim is NOT renewed
/// (it is reclaimable; resurrecting it would race a legitimate recovery).
///
/// Returns `Ok(true)` when renewed, `Ok(false)` on a benign no-op (missing /
/// gone / corrupted / held-by-other / PID-liveness / already-expired claim, or a
/// peer holding the recovery mutex), and `Err(_)` only on a real write failure.
pub fn renew(key: &str, holder: &str, ttl_ms: i64, root: Option<&Path>) -> Result<bool, String> {
    if key.is_empty() || holder.is_empty() {
        return Err("key and holder must be non-empty".into());
    }
    if ttl_ms <= 0 {
        return Err("ttl_ms must be positive".into());
    }
    let path = claim_path(key, root)?;
    // Cheap pre-check outside the mutex: skip the lock for the common
    // not-ours/absent/PID-liveness cases so idle stops stay lock-free.
    match read_claim_file(&path) {
        Ok(rec) if rec.holder == holder && rec.expires_at.is_some() => {}
        Ok(_) => return Ok(false),
        Err(ReadError::GoneAway) => return Ok(false),
        Err(ReadError::Corrupted(_)) => return Ok(false),
    }
    let recovery_lock = path.with_file_name(format!(
        "{}.recovery.d",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    // A peer holding the mutex is mid-reclaim; back off (best-effort) rather
    // than race it. A missed renewal only shortens the lease. But a CORPSE here
    // would block every renewal until some other path cleared it, which is the
    // permanent-wedge shape this mutex's stealing exists to prevent, so retry
    // once past a stale one.
    //
    // Deliberately NOT the bounded ACQUIRE_MAX_ATTEMPTS retry loop `acquire`/
    // `idempotent_reacquire_guarded` use below: real (non-corpse) contention
    // here gives up on this single attempt, same as before this file's other
    // functions gained that loop. That asymmetry is intentional, not a
    // parity gap with Python's refresh_claim (which does retry then raises
    // ClaimContended) - the comment above already justifies it: a caller
    // renews on a regular cadence, so one missed renewal only shortens the
    // lease rather than losing it, unlike a one-shot acquire/refresh call
    // where giving up means the operation itself failed.
    let token = if std::fs::create_dir(&recovery_lock).is_ok() {
        stamp_owner(&recovery_lock)
    } else if steal_if_stale(&recovery_lock) && std::fs::create_dir(&recovery_lock).is_ok() {
        stamp_owner(&recovery_lock)
    } else {
        return Ok(false);
    };
    let result = renew_locked(&path, holder, ttl_ms);
    release_dir_mutex(&recovery_lock, &token);
    result
}

/// The durable session pid: the nearest harness ancestor of THIS process.
///
/// Delegates to `fno agents claim session-pid`, the one implementation of the walk
/// (`cli/src/fno/claims/session_pid.py`). `fno do target init` already shells the
/// same verb to acquire, so re-implementing the ancestry scan here would put two
/// producers on one answer and let them drift.
///
/// Returns `None` on every failure - verb missing, non-numeric output, no
/// harness ancestor - because the caller's fallback is to leave the anchor
/// exactly as it found it. An unresolvable pid is not a reason to write a worse
/// one.
fn durable_session_pid() -> Option<i32> {
    let fno = std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"));
    let mut child = std::process::Command::new(&fno)
        .args(["agents", "claim", "session-pid", "--from-pid"])
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // BOUNDED, because this runs inside the per-claim recovery mutex: an
    // unbounded wait on a slow python start stalls every acquire, refresh and
    // reap contending on the same key. The host has no `timeout` binary, so the
    // bound is native: poll `try_wait`, then kill. A kill degrades to None, and
    // None leaves the anchor exactly as it was found.
    let deadline = std::time::Instant::now() + SESSION_PID_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i32>()
        .ok()
}

/// Wall-clock ceiling on the `claim session-pid` shell-out.
///
/// UNDER the python side's own wait for this same mutex. `compare_and_rebind`
/// gives up after `_RECOVERY_LOCK_MAX_WAIT_S` (5.0s) and `reap`'s targeted
/// recovery waits zero, so a bound above that let a cold python start here hold
/// the lock long enough to make a successor's `fno do target init --handover-from`
/// refuse as mutex-busy, fall through to a plain acquire, and cancel the
/// session on ClaimHeldByOther. Three seconds leaves headroom under 5 and is
/// still ample for a warm resolve; a slower one degrades to None, which leaves
/// the anchor alone.
const SESSION_PID_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Critical section of [`renew`]: re-read under the mutex (the holder may have
/// changed while we grabbed it), then extend only a still-live, still-ours claim.
fn renew_locked(path: &Path, holder: &str, ttl_ms: i64) -> Result<bool, String> {
    let mut existing = match read_claim_file(path) {
        Ok(rec) => rec,
        Err(ReadError::GoneAway) => return Ok(false),
        Err(ReadError::Corrupted(_)) => return Ok(false),
    };
    if existing.holder != holder {
        return Ok(false); // a peer reclaimed it while we took the lock
    }
    if existing.expires_at.is_none() {
        return Ok(false); // PID-liveness claim: no TTL to extend
    }
    if is_expired(&existing, now_ms()) {
        return Ok(false); // reclaimable already; do not resurrect + race recovery
    }
    let now = now_ms();
    // Re-anchor a corpse (x-05be). Guarded three ways: the holder already
    // matched above, the recorded pid must be dead or reused, and the claim must
    // be on THIS machine - off-host we cannot read the pid table, so a
    // dead-looking pid is unverified and only the deadline may move. A LIVE
    // recorded pid is left exactly as it is, so a healthy claim keeps the anchor
    // it was acquired with and this costs nothing on the common path.
    if is_same_machine(&existing.host, existing.machine_id.as_deref()) && !is_live(&existing) {
        // The anchor must be the DURABLE session pid, never this renewer's own.
        // `fno-agents loop-check` is a stop hook that exits in about a second,
        // so anchoring to it would re-file the corpse under a fresh number and
        // fix nothing. `resolve_session_pid` walks up to the nearest harness
        // ancestor and is the single resolver `fno do target init` already uses to
        // acquire (`hooks/helpers/init-target-state.sh` shells the same verb);
        // a second walk implemented here would be two producers of one answer.
        // ONLY when the move actually repairs the claim. acquired_at is held
        // below, and is_live refuses a pid whose create_time is AFTER it, so a
        // RESUMED session's harness (started after the claim was filed) would
        // still classify SUSPECT while overwriting the original holder's pid
        // for nothing. Mirrors `_reanchor_pid_for` in claims/core.py.
        let anchor = durable_session_pid().filter(|pid| {
            process_create_time_ms(*pid).is_some_and(|created| created <= existing.acquired_at)
        });
        if let Some(anchor_pid) = anchor {
            existing.pid = Some(anchor_pid);
            existing.pid_unavailable = false;
            existing.host = hostname();
            let mine = machine_id();
            existing.machine_id = if mine.is_empty() { None } else { Some(mine) };
            // acquired_at STAYS PUT, and the earlier reasoning for moving it
            // was wrong in both directions. Reuse detection compares
            // create_time(pid) against it, and the harness ancestor started
            // BEFORE this renewal, so the original value already reads the
            // anchor as live. Holding it also refuses an anchor whose session
            // began AFTER the claim, which is the cross-session takeover a
            // re-anchor must never perform. And the do provenance row keys
            // started_at on this field, so moving it made the release stamp
            // open a second row instead of closing the one this claim opened.
            // `_rebound_claim(keep_acquired_at=True)` is the python twin.
        }
        // No resolvable durable pid (plain-shell ancestry, or the verb is
        // missing) means no better anchor exists, so the deadline moves alone -
        // byte-for-byte the pre-change behavior rather than a worse guess.
    }
    existing.expires_at = Some(now + ttl_ms);
    let payload = serialize_claim(&existing)?;
    atomic_replace(path, &payload)?;
    Ok(true)
}

/// Process-global lock serializing every test (in ANY module) that mutates OR
/// READS `FNO_CLAIMS_ROOT` / `PATH` / `FNO_BIN`. Env vars are process-global and
/// the crate test suite runs multithreaded, so a per-module lock lets a daemon
/// test and a drive test interleave and clobber each other's env - one shared
/// mutex is the only correct serialization. `cfg(test)` sets crate-wide during
/// `cargo test`, so this is visible to every module's test code.
///
/// READS COUNT, and the word "mutates" alone used to say otherwise. The race is
/// reader-vs-writer, so a lock only writers take excludes nobody: while one test
/// holds `FNO_BIN` pointed at its own stub, every concurrent test that resolves a
/// binary through `$FNO_BIN` silently execs that stub instead of its own. A
/// reader is not exempt just because it leaves the variable as it found it.
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

    #[test]
    fn default_claim_events_path_resolves_from_repo_subdirectory() {
        let td = TempDir::new().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(td.path())
            .status()
            .unwrap()
            .success());
        let nested = td.path().join("crates/fno/src");
        std::fs::create_dir_all(&nested).unwrap();

        // The pure core with no pin, so this asserts the root branch it is named
        // for rather than whatever FNO_EVENTS_PATH the test harness has set.
        assert_eq!(
            claim_events_path_with(None, &nested, None),
            td.path().canonicalize().unwrap().join(".fno/events.jsonl")
        );
    }

    #[test]
    fn claim_events_path_precedence_matches_the_other_two_writers() {
        let td = TempDir::new().unwrap();
        let cwd = td.path();
        let pin = "/tmp/pinned-journal.jsonl";

        // An explicitly named journal outranks the pin.
        assert_eq!(
            claim_events_path_with(Some(Path::new("/explicit")), cwd, Some(pin)),
            PathBuf::from("/explicit/.fno/events.jsonl"),
        );
        // With no explicit journal, the pin wins over the resolved root. This is
        // the leg that keeps Rust on the same file as the Python writer, which
        // the cross-impl merge gate asserts end to end.
        assert_eq!(
            claim_events_path_with(None, cwd, Some(pin)),
            PathBuf::from(pin),
        );
        // An empty pin is not a pin, which is what an exported-but-empty
        // FNO_EVENTS_PATH looks like, and what the other two writers do.
        assert_eq!(
            claim_events_path_with(None, cwd, Some("")),
            crate::paths::worktree_repo_root(cwd).join(".fno/events.jsonl"),
        );
    }

    // ---- lease renewal (x-ba4b) -----------------------------------------

    fn read_claim(root: &TempDir, key: &str) -> ClaimRecord {
        read_claim_file(&lockfile(root, key)).unwrap()
    }

    #[test]
    fn parse_ttl_ms_matches_python_units() {
        // BARE digits are SECONDS (parity with Python _parse_ttl / sleep).
        assert_eq!(parse_ttl_ms("2h"), Some(7_200_000));
        assert_eq!(parse_ttl_ms("30m"), Some(1_800_000));
        assert_eq!(parse_ttl_ms("3600s"), Some(3_600_000));
        assert_eq!(parse_ttl_ms("120"), Some(120_000)); // 120 seconds, not ms
        assert_eq!(parse_ttl_ms("  1h "), Some(3_600_000));
        assert_eq!(parse_ttl_ms(""), None);
        assert_eq!(parse_ttl_ms("abc"), None);
        assert_eq!(parse_ttl_ms("0"), None); // non-positive rejected
    }

    #[test]
    fn renew_resets_deadline_to_now_plus_ttl_and_preserves_acquired_at() {
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        match acquire("node:x-renew", "target-session:me", o) {
            AcquireOutcome::Acquired(_) => {}
            other => panic!("{other:?}"),
        };
        let before = read_claim(&td, "node:x-renew").expires_at.unwrap();
        let acquired_at = read_claim(&td, "node:x-renew").acquired_at;
        std::thread::sleep(Duration::from_millis(2));
        let t0 = now_ms();
        assert_eq!(
            renew(
                "node:x-renew",
                "target-session:me",
                120_000,
                Some(td.path())
            ),
            Ok(true)
        );
        let after = read_claim(&td, "node:x-renew");
        let exp = after.expires_at.unwrap();
        // Deadline is now + ttl (a FIXED window), strictly later than before.
        assert!(exp > before, "before={before} after={exp}");
        assert!(
            (exp - (t0 + 120_000)).abs() < 1_000,
            "deadline must be ~now+ttl, got {exp} vs {}",
            t0 + 120_000
        );
        assert_eq!(after.acquired_at, acquired_at, "acquired_at preserved");
    }

    /// A pid the OS does not report, so `is_live` reads the claim as a corpse.
    fn dead_pid() -> u32 {
        let mut candidate = 999_999u32;
        while std::path::Path::new(&format!("/proc/{candidate}")).exists()
            || unsafe { libc::kill(candidate as i32, 0) } == 0
        {
            candidate += 1;
        }
        candidate
    }

    /// Point `FNO_BIN` at a stub answering `claim session-pid` with `pid`.
    /// An empty `pid` reproduces the no-harness-ancestor degrade, which the
    /// real verb signals with empty stdout and exit 0.
    fn stub_session_pid(dir: &std::path::Path, pid: &str) -> PathBuf {
        let script = dir.join("fno-stub");
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s' '{pid}'\n")).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[test]
    fn renew_reanchors_a_dead_pid_to_the_durable_session_pid() {
        // x-05be: preserving a corpse is what made SUSPECT mean two things. A
        // respawned worker and a dead one left byte-identical claims.
        let _guard = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        o.pid = Some(dead_pid());
        let _ = acquire("node:x-corpse", "target-session:me", o);
        assert_eq!(
            classify(&read_claim(&td, "node:x-corpse"), None),
            ClaimState::Suspect,
            "fixture must start SUSPECT or this proves nothing"
        );

        let stub = stub_session_pid(td.path(), &std::process::id().to_string());
        std::env::set_var("FNO_BIN", &stub);
        let result = renew(
            "node:x-corpse",
            "target-session:me",
            120_000,
            Some(td.path()),
        );
        std::env::remove_var("FNO_BIN");
        assert_eq!(result, Ok(true));

        let after = read_claim(&td, "node:x-corpse");
        assert_eq!(after.pid, Some(std::process::id() as i32));
        assert_eq!(
            classify(&after, None),
            ClaimState::Live,
            "a re-anchored claim must read LIVE, not SUSPECT"
        );
    }

    #[test]
    fn renew_holds_acquired_at_while_re_anchoring_the_pid() {
        // acquired_at STAYS PUT. Reuse detection compares create_time(pid)
        // against it, and the anchor started BEFORE the claim, so the original
        // value already reads the anchor as live - asserted below by
        // classifying LIVE, not by reading the field alone.
        //
        // Two things need it to hold still. The do provenance row keys
        // started_at on this field, so moving it made the release stamp open a
        // second row instead of closing the one this claim opened. And a fixed
        // anchor refuses a session that began AFTER the claim, which is exactly
        // the cross-session takeover a re-anchor must never perform.
        let _guard = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        o.pid = Some(dead_pid());
        let _ = acquire("node:x-anchor", "target-session:me", o);
        let before = read_claim(&td, "node:x-anchor").acquired_at;
        std::thread::sleep(Duration::from_millis(2));

        let stub = stub_session_pid(td.path(), &std::process::id().to_string());
        std::env::set_var("FNO_BIN", &stub);
        let _ = renew(
            "node:x-anchor",
            "target-session:me",
            120_000,
            Some(td.path()),
        );
        std::env::remove_var("FNO_BIN");

        let after = read_claim(&td, "node:x-anchor");
        assert_eq!(
            after.acquired_at, before,
            "acquired_at moved; the do row keys started_at on it"
        );
        assert_eq!(
            after.pid,
            Some(std::process::id() as i32),
            "the pid must still be re-anchored"
        );
        assert_eq!(
            classify(&after, None),
            ClaimState::Live,
            "a held anchor must still read LIVE, or the repair did nothing"
        );
    }

    #[test]
    fn renew_leaves_the_anchor_alone_with_no_harness_ancestor() {
        // No better anchor exists, and this renewer's own pid is a worse one:
        // loop-check exits about a second after renewing.
        let _guard = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        let corpse = dead_pid();
        o.pid = Some(corpse);
        let _ = acquire("node:x-noanchor", "target-session:me", o);
        let before = read_claim(&td, "node:x-noanchor");
        // now_ms() has millisecond resolution, so without this the acquire and
        // the renew can land in the SAME millisecond and the strict deadline
        // comparison below flakes. The sibling deadline tests sleep for the
        // same reason.
        std::thread::sleep(Duration::from_millis(2));

        let stub = stub_session_pid(td.path(), "");
        std::env::set_var("FNO_BIN", &stub);
        let result = renew(
            "node:x-noanchor",
            "target-session:me",
            120_000,
            Some(td.path()),
        );
        std::env::remove_var("FNO_BIN");
        assert_eq!(result, Ok(true));

        let after = read_claim(&td, "node:x-noanchor");
        assert_eq!(after.pid, Some(corpse as i32));
        assert_eq!(after.acquired_at, before.acquired_at);
        assert!(after.expires_at.unwrap() > before.expires_at.unwrap());
    }

    #[test]
    fn renew_deadline_does_not_grow_across_repeated_renewals() {
        // Regression for the span-growth bug (codex P1): renewing N times must
        // NOT compound the window. Each renewal pins expires_at to now+ttl.
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(120_000);
        let _ = acquire("node:x-grow", "target-session:me", o);
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(2));
            assert_eq!(
                renew("node:x-grow", "target-session:me", 120_000, Some(td.path())),
                Ok(true)
            );
        }
        let exp = read_claim(&td, "node:x-grow").expires_at.unwrap();
        // After 5 renewals the deadline is still ~now+120s, NOT now+(5*elapsed)+120s.
        assert!(
            exp - now_ms() < 121_000,
            "deadline grew across renewals: {}ms out",
            exp - now_ms()
        );
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
            renew(
                "node:x-other",
                "target-session:intruder",
                120_000,
                Some(td.path())
            ),
            Ok(false)
        );
        assert_eq!(read_claim(&td, "node:x-other").expires_at.unwrap(), before);
    }

    #[test]
    fn renew_is_noop_for_expired_pid_liveness_and_missing_claim() {
        let td = TempDir::new().unwrap();
        // Missing claim -> Ok(false).
        assert_eq!(
            renew("node:x-absent", "h", 120_000, Some(td.path())),
            Ok(false)
        );
        // PID-liveness claim (no ttl_ms) has no expires_at to extend -> Ok(false).
        let _ = acquire("session:pidonly", "h", opts_in(&td));
        assert!(read_claim(&td, "session:pidonly").expires_at.is_none());
        assert_eq!(
            renew("session:pidonly", "h", 120_000, Some(td.path())),
            Ok(false)
        );
        // An already-expired claim is NOT resurrected (it is reclaimable).
        let mut o = opts_in(&td);
        o.ttl_ms = Some(60_000);
        let _ = acquire("node:x-expired", "target-session:me", o);
        // Hand-write an expired deadline for our own claim.
        let mut rec = read_claim(&td, "node:x-expired");
        rec.expires_at = Some(now_ms() - 1);
        atomic_replace(
            &lockfile(&td, "node:x-expired"),
            &serialize_claim(&rec).unwrap(),
        )
        .unwrap();
        assert_eq!(
            renew(
                "node:x-expired",
                "target-session:me",
                60_000,
                Some(td.path())
            ),
            Ok(false)
        );
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
    fn ttl_claim_with_no_pid_records_explicit_unavailability() {
        let td = TempDir::new().unwrap();
        let mut o = opts_in(&td);
        o.ttl_ms = Some(60_000);
        o.pid_unavailable = true;
        let rec = match acquire("session:u3", "pty:cc", o) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rec.pid, None);
        assert!(rec.pid_unavailable);
        assert_eq!(rec.schema_version, 2);
        let text = std::fs::read_to_string(lockfile(&td, "session:u3")).unwrap();
        assert!(text.contains("pid: null"));
        assert!(text.contains("pid_unavailable: true"));
    }

    #[test]
    fn pid_unavailable_without_ttl_is_rejected() {
        let rec = parse_claim_str(
            "schema_version: 2\nkey: k\nholder: h\nacquired_at: 5\npid: null\npid_unavailable: true\nhost: x\n",
        );
        assert!(rec.is_err());
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
            "schema_version: 3\nkey: k\nholder: h\nacquired_at: 5\npid: 1\nhost: x\n",
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
            pid: Some(7),
            host: "hh".into(),
            pid_unavailable: false,
            expires_at: None,
            reason: Some("why".into()),
            harness: Some("codex".into()),
            pid_provenance: Some("session-prover".into()),
            machine_id: Some("mid".into()),
            metadata: meta,
        };
        let text = serialize_claim(&rec).unwrap();
        let back = parse_claim_str(&text).unwrap();
        assert_eq!(back, rec);
    }

    // ---- harness tag (x-3e70) ---------------------------------------------

    // AC6-FR: a claim record written before this change (no `harness` key)
    // parses with `harness: None` and does not crash.
    #[test]
    fn claim_without_harness_key_reads_none() {
        let yaml = "schema_version: 1\nkey: node:x\nholder: h\nacquired_at: 1\npid: 2\nhost: hh\n";
        let rec = parse_claim_str(yaml).expect("legacy record must parse");
        assert_eq!(rec.harness, None);
    }

    // A record WITH a harness key round-trips it back.
    #[test]
    fn claim_with_harness_key_round_trips() {
        let yaml = "schema_version: 1\nkey: node:x\nholder: h\nacquired_at: 1\npid: 2\nhost: hh\nharness: codex\n";
        let rec = parse_claim_str(yaml).expect("record must parse");
        assert_eq!(rec.harness.as_deref(), Some("codex"));
        // None is omitted from output entirely (not serialized as null).
        let none = ClaimRecord {
            harness: None,
            ..rec.clone()
        };
        assert!(!serialize_claim(&none).unwrap().contains("harness"));
    }

    #[test]
    fn resolve_harness_single_family_wins_disagreement_is_unknown() {
        // Two DISAGREEING families are ambiguous: precedence must not pick codex
        // and tag the claim with a harness this process cannot prove it owns.
        let both = |k: &str| match k {
            "CODEX_THREAD_ID" => Some("cx".to_string()),
            "CLAUDE_CODE_SESSION_ID" => Some("cl".to_string()),
            _ => None,
        };
        assert_eq!(resolve_harness_from(both).as_deref(), None);
        // Two markers of ONE family agree -> that family, not ambiguous.
        let same_family = |k: &str| match k {
            "CODEX_THREAD_ID" => Some("cx".to_string()),
            "CODEX_SESSION_ID" => Some("cx2".to_string()),
            _ => None,
        };
        assert_eq!(resolve_harness_from(same_family).as_deref(), Some("codex"));
        // A blank higher-precedence marker is UNSET; a lower real one still wins.
        let blank_hi = |k: &str| match k {
            "CODEX_THREAD_ID" => Some("   ".to_string()),
            "CLAUDE_CODE_SESSION_ID" => Some("cl".to_string()),
            _ => None,
        };
        assert_eq!(resolve_harness_from(blank_hi).as_deref(), Some("claude"));
        assert_eq!(
            resolve_harness_from(|k| (k == "OPENCODE_SESSION_ID").then(|| "ses_1".to_string()))
                .as_deref(),
            Some("opencode")
        );
        // No markers -> None (unknown), never a panic.
        assert_eq!(resolve_harness_from(|_| None), None);
    }

    #[test]
    fn ambient_parent_edge_resolves_id_harness_and_refuses_mixing() {
        // Single claude marker: the VALUE is the parent session id.
        let claude = |k: &str| match k {
            "CLAUDE_CODE_SESSION_ID" => Some("7420e8f7-eeba".to_string()),
            _ => None,
        };
        assert_eq!(
            ambient_parent_edge_from(claude),
            (
                Some("7420e8f7-eeba".to_string()),
                Some("claude".to_string()),
                ambient_parent_edge_from(|_| None).2
            )
        );
        // Within the codex family the thread id wins over the legacy var, and
        // the winner's value (not the loser's) is the parent id.
        let codex = |k: &str| match k {
            "CODEX_THREAD_ID" => Some("t-1".to_string()),
            "CODEX_SESSION_ID" => Some("legacy-1".to_string()),
            _ => None,
        };
        assert_eq!(
            ambient_parent_edge_from(codex),
            (
                Some("t-1".to_string()),
                Some("codex".to_string()),
                ambient_parent_edge_from(|_| None).2
            )
        );
        // Two DISAGREEING families attribute NOTHING: no laundered lineage.
        let mixed = |k: &str| match k {
            "CODEX_THREAD_ID" => Some("cx".to_string()),
            "CLAUDE_CODE_SESSION_ID" => Some("cl".to_string()),
            _ => None,
        };
        let (s, h, c) = ambient_parent_edge_from(mixed);
        assert_eq!((s, h), (None, None));
        assert!(c.is_some(), "cwd is captured even when identity is refused");
        // No markers: identity absent, cwd still present, never a panic.
        let (s, h, _) = ambient_parent_edge_from(|_| None);
        assert_eq!((s, h), (None, None));
    }

    // ---- liveness classification (contract item 8) ------------------------

    fn record(pid: i32, acquired_at: i64, expires_at: Option<i64>, host: &str) -> ClaimRecord {
        ClaimRecord {
            schema_version: 1,
            key: "session:x".into(),
            holder: "h".into(),
            acquired_at,
            pid: Some(pid),
            host: host.into(),
            pid_unavailable: false,
            expires_at,
            reason: None,
            harness: None,
            pid_provenance: None,
            // None on purpose: these fixtures pass a HOST, so they exercise the
            // pre-change fallback arm. The machine-id arm has its own tests.
            machine_id: None,
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
            classify(
                &record(me, now, Some(now + 60_000), "elsewhere.example"),
                Some(now)
            ),
            ClaimState::Suspect
        );
        // HYBRID arm, corroborated: expired TTL + live PROVER-PROVEN pid ->
        // LIVE. The fixture's None provenance cannot reach LIVE on expiry, so
        // prove it explicitly here.
        let mut proven = record(me, now, Some(now - 1), &host);
        proven.pid_provenance = Some("session-prover".into());
        assert_eq!(classify(&proven, Some(now)), ClaimState::Live);
        // Expired TTL + live pid WITHOUT provenance (legacy / ambient, the
        // foreign-pid specimen shape) -> STALE: the TTL is a lease.
        assert_eq!(
            classify(&record(me, now, Some(now - 1), &host), Some(now)),
            ClaimState::Stale
        );
        // Expired TTL + dead pid -> STALE.
        assert_eq!(
            classify(&record(-1, now, Some(now - 1), &host), Some(now)),
            ClaimState::Stale
        );
    }

    #[test]
    fn make_claim_stamps_pid_provenance_by_pid_origin() {
        let td = TempDir::new().unwrap();
        // A defaulted pid is the claimant itself: the strongest provenance.
        let own = match acquire("session:prov", "pty:me", opts_in(&td)) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(own.pid_provenance.as_deref(), Some("session-prover"));
        // An explicitly passed pid is caller-supplied and unverifiable here:
        // ambient, so the hybrid arm will not extend an expired lease for it.
        let mut o = opts_in(&td);
        o.pid = Some(4242);
        let foreign = match acquire("session:prov2", "pty:me", o) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(foreign.pid_provenance.as_deref(), Some("ambient"));
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
        assert_eq!(rec.pid, Some(std::process::id() as i32));
        assert!(lockfile(&td, "session:fresh").exists());
        let events = read_events(&td);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "claim_acquired");
        assert_eq!(events[0]["source"], "fno-loop");
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
        assert_eq!(second.pid, Some(4242));
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
                assert_eq!(pid, Some(std::process::id() as i32));
            }
            other => panic!("{other:?}"),
        }
    }

    // -- machine identity -------------------------------------

    #[test]
    fn is_same_machine_host_arm() {
        // The pre-change fallback, used when no machine id was recorded. Always
        // expressible, with or without an OS machine id on this box.
        assert!(is_same_machine(&hostname(), None));
        assert!(!is_same_machine("", None));
        assert!(!is_same_machine(
            "some-other-host-that-does-not-exist",
            None
        ));
    }

    #[test]
    fn is_same_machine_machine_arm() {
        // Needs a real OS id: where none exists both writers omit the field and
        // the machine arm cannot be exercised at all.
        if machine_id().is_empty() {
            return;
        }
        assert!(is_same_machine("anything", Some(&machine_id())));
        assert!(!is_same_machine(
            &hostname(),
            Some("00000000-0000-0000-0000-000000000000")
        ));
    }

    #[test]
    fn unknown_own_machine_id_is_not_foreign() {
        // Mirrors the Python contract: a claim naming a machine, read where our
        // own id is unreadable, must not read as foreign - that would stale a
        // live local claim, and ambiguity degrades to skip, never to steal.
        if !machine_id().is_empty() {
            return; // this box HAS an id, so the unknown path is unreachable here
        }
        assert!(is_same_machine(
            "a-name-it-no-longer-has",
            Some("some-machine-id")
        ));
    }

    #[test]
    fn machine_id_is_stable_across_calls() {
        // The whole point: a value that cannot move mid-session. A hostname
        // can (DHCP/DNS/VPN/sleep-wake on macOS), which is what made a live
        // holder read cross-host -> stale -> stealable.
        assert_eq!(machine_id(), machine_id());
    }

    #[test]
    fn make_claim_records_machine_id_not_hostname() {
        // Parity guard: Python's acquire writes the same value. If these two
        // writers disagree, each implementation reads the other's claims as
        // cross-machine and silently treats live claims as recoverable. Where
        // the OS exposes no id both omit the field, which is also parity.
        let td = TempDir::new().unwrap();
        match acquire("session:mid", "pty:owner", opts_in(&td)) {
            AcquireOutcome::Acquired(rec) => {
                let expected = machine_id();
                if expected.is_empty() {
                    assert_eq!(rec.machine_id, None);
                } else {
                    assert_eq!(rec.machine_id.as_deref(), Some(expected.as_str()));
                }
                assert_eq!(
                    rec.host,
                    hostname(),
                    "host stays the hostname a pre-change reader expects"
                );
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
    fn idempotent_reacquire_waits_for_held_recovery_mutex_and_revalidates() {
        let td = TempDir::new().unwrap();
        let first = match acquire("session:idem-race", "pty:me", opts_in(&td)) {
            AcquireOutcome::Acquired(r) => r,
            other => panic!("{other:?}"),
        };
        let path = lockfile(&td, "session:idem-race");
        let mutex = path.with_file_name(format!(
            "{}.recovery.d",
            path.file_name().unwrap().to_string_lossy()
        ));
        // Simulate another worker actively recovering this key.
        std::fs::create_dir(&mutex).unwrap();

        let mut o = opts_in(&td);
        o.pid = Some(4242);
        let racer = std::thread::spawn(move || acquire("session:idem-race", "pty:me", o));

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !racer.is_finished(),
            "idempotent re-acquire must block on the held recovery mutex"
        );
        let still_old = read_claim_file(&path).unwrap();
        assert_eq!(
            still_old.pid, first.pid,
            "the on-disk claim must not be rewritten while the recovery mutex is held"
        );

        // A DIFFERENT holder wins the key entirely while the racer waits -
        // the exact scenario the mutex exists to serialize against.
        let mut other = record(
            std::process::id() as i32,
            first.acquired_at + 1,
            None,
            &hostname(),
        );
        other.key = "session:idem-race".into();
        other.holder = "pty:other".into();
        std::fs::write(&path, serialize_claim(&other).unwrap()).unwrap();
        std::fs::remove_dir(&mutex).unwrap();

        match racer.join().unwrap() {
            AcquireOutcome::HeldByOther { holder, .. } => assert_eq!(holder, "pty:other"),
            outcome => panic!("must not overwrite the new holder's live claim: {outcome:?}"),
        }
    }

    #[test]
    fn deadline_expired_waiter_never_steals_a_fresh_recovery_mutex() {
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
        // Held for the whole call and young enough to be an honest holder:
        // acquire retries rather than stealing (an in-place steal would
        // reintroduce the TOCTOU double-winner), and the stale claim is
        // untouched. Only a mutex past STALE_MUTEX_STEAL is taken, by rename.
        wait_for_recovery_release(&mutex, Duration::from_millis(50)); // exercise the wait path cheaply
        let out = recover_stale(&path, "session:x", "pty:thief", &opts_in(&td), None);
        assert!(matches!(out, RecoverResult::Retry));
        assert!(mutex.exists(), "recovery mutex was stolen");
        let kept = read_claim_file(&path).ok().unwrap();
        assert_eq!(kept.holder, "h");
    }

    /// Backdate a lock dir's mtime, which is what the steal predicate reads.
    /// Via libc (already a direct dependency) rather than pulling in filetime.
    fn age_dir(path: &Path, secs: u64) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let t = libc::timeval {
            tv_sec: now - secs as i64,
            tv_usec: 0,
        };
        let times = [t, t];
        // lutimes, not utimes: identical for a real dir, and the only one that
        // works on the dangling-symlink case below.
        assert_eq!(unsafe { libc::lutimes(c.as_ptr(), times.as_ptr()) }, 0);
    }

    #[test]
    fn restore_grace_is_shorter_than_the_steal_threshold() {
        // Twin of the Python test_AC7_EDGE: a grace >= the threshold backdates
        // into the future, permanently un-stealable.
        assert!(RESTORE_GRACE < STALE_MUTEX_STEAL);
        assert!(RESTORE_GRACE > Duration::ZERO);
    }

    #[test]
    fn backdate_mtime_grants_a_grace_window_not_a_fresh_reprieve() {
        // Twin of the Python test_AC2_HP: a dir backdated by
        // STALE_MUTEX_STEAL - RESTORE_GRACE becomes stealable again only after
        // RESTORE_GRACE elapses, not the full threshold.
        let td = TempDir::new().unwrap();
        let dir = td.path().join("a.lock.d");
        std::fs::create_dir(&dir).unwrap();

        backdate_mtime(&dir, STALE_MUTEX_STEAL - RESTORE_GRACE);

        let age = std::fs::symlink_metadata(&dir)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        // Not yet stale: still inside the grace window.
        assert!(age <= STALE_MUTEX_STEAL, "backdated dir is already stale");
        // But within a second of the threshold, i.e. the grace window is
        // nearly spent, not a full fresh STALE_MUTEX_STEAL of protection.
        assert!(
            STALE_MUTEX_STEAL - age <= RESTORE_GRACE + Duration::from_secs(1),
            "backdated dir kept more than a grace window of protection: {age:?} old, \
             threshold {STALE_MUTEX_STEAL:?}"
        );
    }

    #[test]
    fn recovery_mutex_corpse_is_stolen_so_a_claim_cannot_brick() {
        // The permanence mechanism of the Jul 13 outage: a recoverer died
        // holding the mutex, so the stale claim could never be reclaimed.
        let td = TempDir::new().unwrap();
        let path = lockfile(&td, "session:x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stale = record(999_999, 1, None, &hostname());
        std::fs::write(&path, serialize_claim(&stale).unwrap()).unwrap();
        let mutex = path.with_file_name(format!(
            "{}.recovery.d",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir(&mutex).unwrap();
        age_dir(&mutex, STALE_MUTEX_STEAL.as_secs() + 60);

        let out = acquire("session:x", "pty:heir", opts_in(&td));

        assert!(matches!(out, AcquireOutcome::Acquired(_)), "{out:?}");
        assert!(!mutex.exists(), "corpse survived the steal");
    }

    #[test]
    fn events_lock_corpse_is_stolen_within_the_daemon_budget() {
        // AC5-ERR: the 2s hot-path budget still holds -- a corpse is stolen on
        // the first spin rather than burning the whole deadline.
        let td = TempDir::new().unwrap();
        let events = td.path().join(".fno/events.jsonl");
        std::fs::create_dir_all(events.parent().unwrap()).unwrap();
        let lock = events.with_file_name("events.jsonl.lock.d");
        std::fs::create_dir(&lock).unwrap();
        age_dir(&lock, STALE_MUTEX_STEAL.as_secs() + 60);

        let started = Instant::now();
        let res = append_event_line(
            &events,
            &json!({"ts": "t", "type": "x"}),
            Duration::from_secs(2),
        );

        assert!(res.is_ok(), "{res:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!lock.exists());
        assert_eq!(std::fs::read_to_string(&events).unwrap().lines().count(), 1);
    }

    #[test]
    fn dangling_symlink_lock_never_spins() {
        // EEXIST to create_dir but NotFound to a following metadata(): a
        // "retry now" there spins the caller forever with its deadline
        // unreachable. Stale -> stolen; fresh -> waited on.
        let td = TempDir::new().unwrap();
        let lock = td.path().join("events.jsonl.lock.d");
        std::os::unix::fs::symlink(td.path().join("nonexistent"), &lock).unwrap();

        assert!(!steal_if_stale(&lock), "fresh dangling link was stolen");

        age_dir(&lock, STALE_MUTEX_STEAL.as_secs() + 60);
        assert!(steal_if_stale(&lock), "stale dangling link was not stolen");
        assert!(std::fs::symlink_metadata(&lock).is_err());
    }

    #[test]
    fn repeated_steals_never_collide_on_the_reap_name() {
        let td = TempDir::new().unwrap();
        let lock = td.path().join("events.jsonl.lock.d");
        for _ in 0..3 {
            std::fs::create_dir(&lock).unwrap();
            age_dir(&lock, STALE_MUTEX_STEAL.as_secs() + 60);
            assert!(steal_if_stale(&lock));
            assert!(!lock.exists());
        }
    }

    #[test]
    fn events_lock_fresh_contention_still_times_out() {
        // AC2-EDGE: honest contention keeps today's log-and-skip behavior.
        let td = TempDir::new().unwrap();
        let events = td.path().join(".fno/events.jsonl");
        std::fs::create_dir_all(events.parent().unwrap()).unwrap();
        std::fs::create_dir(events.with_file_name("events.jsonl.lock.d")).unwrap();

        let res = append_event_line(
            &events,
            &json!({"ts": "t", "type": "x"}),
            Duration::from_secs(2),
        );

        assert!(res.is_err(), "fresh lock was stolen");
    }

    #[test]
    fn event_append_retries_when_setup_retargets_leaf_while_waiting() {
        let td = TempDir::new().unwrap();
        let local = td.path().join("worktree-events.jsonl");
        std::fs::write(&local, b"").unwrap();
        let canonical = td.path().join("canonical-events.jsonl");
        std::fs::write(&canonical, b"").unwrap();
        let local_lock = td.path().join("worktree-events.jsonl.lock.d");
        let canonical_lock = td.path().join("canonical-events.jsonl.lock.d");
        std::fs::create_dir(&local_lock).unwrap();
        std::fs::create_dir(&canonical_lock).unwrap();

        let writer_path = local.clone();
        let writer = std::thread::spawn(move || {
            append_event_line(
                &writer_path,
                &json!({"ts": "t", "type": "handoff"}),
                Duration::from_secs(5),
            )
        });
        std::thread::sleep(Duration::from_millis(100));
        std::fs::rename(&local, td.path().join("local-backup.jsonl")).unwrap();
        std::os::unix::fs::symlink(&canonical, &local).unwrap();
        std::fs::remove_dir_all(&local_lock).unwrap();

        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            std::fs::metadata(&canonical).unwrap().len(),
            0,
            "writer bypassed the canonical mutex after the symlink handoff"
        );

        std::fs::remove_dir_all(&canonical_lock).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&canonical).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn release_after_steal_leaves_new_holder_intact() {
        // AC2: a holder whose lock was stolen mid-write must not delete the new
        // holder's lock on release. This is the wrongful-delete vector the owner
        // token exists to close (twin of the Python test_mutex_steal AC2 test).
        let td = TempDir::new().unwrap();
        let lock = td.path().join("events.jsonl.lock.d");

        // Victim acquires, then is suspended past the steal threshold.
        let victim = acquire_dir_mutex(&lock, Duration::from_secs(5), true).unwrap();
        age_dir(&lock, STALE_MUTEX_STEAL.as_secs() + 60);

        // A stealer reaps the corpse, then a new holder acquires at the path.
        assert!(steal_if_stale(&lock));
        assert!(!lock.exists());
        let new_holder = acquire_dir_mutex(&lock, Duration::from_secs(5), true).unwrap();
        assert_ne!(new_holder, victim);

        // Victim resumes and releases: the new holder's lock must survive.
        release_dir_mutex(&lock, &victim);
        assert!(
            lock.exists(),
            "victim's release deleted the new holder's lock"
        );

        // The new holder releases cleanly (token matches -> remove_dir_all).
        release_dir_mutex(&lock, &new_holder);
        assert!(!lock.exists());
    }

    #[test]
    fn renewal_after_age_read_is_not_classified_as_stale() {
        let td = TempDir::new().unwrap();
        let lock = td.path().join("events.jsonl.lock.d");
        let token = acquire_dir_mutex(&lock, Duration::from_secs(5), true).unwrap();
        age_dir(&lock, STALE_MUTEX_STEAL.as_secs() + 60);
        let before = std::fs::symlink_metadata(&lock).unwrap();
        let before_modified = before.modified().ok();

        age_dir(&lock, 0);
        assert_eq!(read_owner(&lock), token);
        assert!(!same_stale_lease(&lock, &token, before_modified));

        release_dir_mutex(&lock, &token);
    }

    #[test]
    fn concurrent_stealers_have_exactly_one_rename_winner() {
        // AC3-FR: both writers land whole lines; neither deadlocks.
        let td = TempDir::new().unwrap();
        let events = td.path().join(".fno/events.jsonl");
        std::fs::create_dir_all(events.parent().unwrap()).unwrap();
        let lock = events.with_file_name("events.jsonl.lock.d");
        std::fs::create_dir(&lock).unwrap();
        age_dir(&lock, STALE_MUTEX_STEAL.as_secs() + 60);

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let events = events.clone();
                std::thread::spawn(move || {
                    append_event_line(
                        &events,
                        &json!({"ts": "t", "type": "x", "i": i}),
                        // The assertion is that all four lines land whole with
                        // one rename winner, never that they land fast, so the
                        // budget is generous. But it must EXCEED STALE_MUTEX_STEAL,
                        // not equal it. steal_if_stale is age-gated, so a FRESH
                        // holder is never stolen and always releases; the only
                        // delay past normal contention is a holder starved under
                        // parallel load, whose lock becomes stealable at exactly
                        // STALE_MUTEX_STEAL. A budget == the threshold (the prior
                        // 120s == 120s) puts the steal and the waiter's deadline on
                        // the same knife-edge, so the deadline wins and the test
                        // flakes at 120s wall. 2x gives the steal a full threshold
                        // of headroom before the deadline.
                        STALE_MUTEX_STEAL * 2,
                    )
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }

        assert_eq!(std::fs::read_to_string(&events).unwrap().lines().count(), 4);
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
