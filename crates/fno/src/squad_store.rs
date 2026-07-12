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
//! write returns an error the caller notices once. The env override
//! (`FNO_AGENTS_HOME`) redirects the file so tests never touch a real home,
//! mirroring [`crate::agents_view::registry_path`].

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A process-global lock for tests that redirect `FNO_AGENTS_HOME` (the env
/// var is shared, so every store-touching test across the crate must serialize
/// on ONE mutex, not per-module ones).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    squads: Vec<StoredSquad>,
}

/// What [`load`] read: the (member-validated) squads plus an optional one-line
/// notice for the operator (a quarantine, or dropped hostile ids).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub squads: Vec<StoredSquad>,
    pub notice: Option<String>,
}

/// The store file: a sibling of the registry under `FNO_AGENTS_HOME`, else
/// `$HOME/.fno/squads.json`. Machine-global because a squad spans repos.
pub fn squads_path() -> PathBuf {
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
                squads: Vec::new(),
                notice: Some(format!("quarantined corrupt squads.json to {}", aside.display())),
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
    Loaded {
        squads,
        notice: (dropped > 0).then(|| format!("dropped {dropped} malformed squad member(s)")),
    }
}

/// Insert-or-replace the entry named `name`, preserving its `created_at` if it
/// already exists (else stamping now). Write-through for `NewSquad`, recruit,
/// member close, and tombstone.
pub fn upsert(name: &str, origins: &[String], members: &[StoredMember]) -> io::Result<()> {
    mutate(|squads| {
        let created_at = squads
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.created_at.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(now_iso);
        squads.retain(|s| s.name != name);
        squads.push(StoredSquad {
            name: name.to_string(),
            origins: origins.to_vec(),
            members: members.to_vec(),
            created_at,
        });
    })
}

/// Delete the entry named `name` (a user-closed / removed workspace). A name
/// not present is a silent no-op.
pub fn remove(name: &str) -> io::Result<()> {
    mutate(|squads| squads.retain(|s| s.name != name))
}

/// Rename `old` -> `new` in one locked mutation, carrying `created_at` across
/// (a `RenameSquad`). Any pre-existing `new` entry is overwritten.
pub fn rename(old: &str, new: &str, origins: &[String], members: &[StoredMember]) -> io::Result<()> {
    mutate(|squads| {
        let created_at = squads
            .iter()
            .find(|s| s.name == old || s.name == new)
            .map(|s| s.created_at.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(now_iso);
        squads.retain(|s| s.name != old && s.name != new);
        squads.push(StoredSquad {
            name: new.to_string(),
            origins: origins.to_vec(),
            members: members.to_vec(),
            created_at,
        });
    })
}

/// The locked read-modify-write core: acquire an exclusive, NON-BLOCKING lock
/// on a sibling lockfile (bounded retry, then give up), re-read the current
/// file, apply `f`, and atomically rename a tmp over the target. Two mux
/// servers serialize on the lockfile; last writer wins per squad name. A
/// corrupt file read here is treated as empty (the load path owns quarantine),
/// so a write never fails on unreadable prior content.
fn mutate(f: impl FnOnce(&mut Vec<StoredSquad>)) -> io::Result<()> {
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

    let mut squads = match std::fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str::<StoreFile>(&raw)
            .map(|sf| sf.squads)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    f(&mut squads);

    let file = StoreFile {
        version: STORE_VERSION,
        squads,
    };
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
struct FlockGuard(std::fs::File);

impl FlockGuard {
    fn acquire(file: std::fs::File) -> io::Result<Self> {
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

    /// A scratch `FNO_AGENTS_HOME` so the store never touches a real file. The
    /// env var is process-global, so these tests are serialized by the mutex.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fno-squadstore-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("FNO_AGENTS_HOME", &dir);
            Scratch(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("squads.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::env::remove_var("FNO_AGENTS_HOME");
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The env var is global; hold this across every test that sets it.
    use super::TEST_ENV_LOCK as ENV_LOCK;

    fn m(id: &str) -> StoredMember {
        StoredMember {
            attach_id: id.into(),
            tombstone: false,
        }
    }

    #[test]
    fn missing_file_is_a_fresh_store() {
        let _g = ENV_LOCK.lock().unwrap();
        let _s = Scratch::new("missing");
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.is_none(), "a missing file is silent");
    }

    #[test]
    fn upsert_then_load_roundtrips_and_preserves_created_at() {
        let _g = ENV_LOCK.lock().unwrap();
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
    fn corrupt_file_is_quarantined_and_read_empty() {
        // AC1-ERR: invalid JSON is renamed aside, not fatal.
        let _g = ENV_LOCK.lock().unwrap();
        let s = Scratch::new("corrupt");
        std::fs::write(s.file(), "{not valid json").unwrap();
        let loaded = load();
        assert!(loaded.squads.is_empty());
        assert!(loaded.notice.as_deref().unwrap().contains("quarantined"));
        assert!(!s.file().exists(), "the corrupt file was moved aside");
        let asides: Vec<_> = std::fs::read_dir(&s.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("squads.json.corrupt-"))
            .collect();
        assert_eq!(asides.len(), 1, "exactly one quarantine file");
    }

    #[test]
    fn unknown_version_is_quarantined() {
        // Discretion 5: a version this build does not understand takes the
        // quarantine path, never a best-effort parse.
        let _g = ENV_LOCK.lock().unwrap();
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
        let _g = ENV_LOCK.lock().unwrap();
        let s = Scratch::new("hostile");
        let file = StoreFile {
            version: STORE_VERSION,
            squads: vec![StoredSquad {
                name: "w".into(),
                origins: vec![],
                members: vec![
                    m("c19cd2c3"),          // good
                    m("; rm -rf"),          // shell metachar
                    m("deadbeef9"),         // 9 chars
                    m("GHIJKLmn"),          // non-hex
                ],
                created_at: "2026-07-11T00:00:00Z".into(),
            }],
        };
        std::fs::write(s.file(), serde_json::to_string(&file).unwrap()).unwrap();
        let loaded = load();
        assert_eq!(loaded.squads[0].members, vec![m("c19cd2c3")]);
        assert!(loaded.notice.as_deref().unwrap().contains("dropped 3"));
    }

    #[test]
    fn remove_and_rename_mutate_by_name() {
        let _g = ENV_LOCK.lock().unwrap();
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
}
