//! Release-on-stop (x-9c91 change 5): stopping a worker releases its claims.
//! One implementation serves the daemon (direct call) and the Python leg (through the release-stopped op), so no parity guard is needed.

use crate::claims::{
    common_event_data, emit_audit_event, encode_key, is_same_machine, list_in_result, now_ms,
    probe_pid, read_claim_file, ClaimRecord, PidProbe, ReadError,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Release-on-stop (x-9c91 change 5): stopping a worker releases its claims
// ---------------------------------------------------------------------------

/// The stopped worker whose claims a stop/rm releases: the worker NAME and,
/// when the registry row carried one, its harness session id.
pub struct StoppedHolder {
    pub name: String,
    pub harness_session_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StopReleasedClaim {
    pub key: String,
    pub holder: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StopKeptClaim {
    pub key: String,
    pub holder: String,
    /// The observed reading that kept this claim: `pid 123 present`,
    /// `pid 123 access-denied`, or `off-host <host>`. A measurement, never
    /// an inference from an absence.
    pub observed: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct StopReleaseReceipt {
    pub released: Vec<StopReleasedClaim>,
    pub kept: Vec<StopKeptClaim>,
    pub scanned: usize,
    pub dirs: Vec<PathBuf>,
    /// The directories the scan actually read, a subset of `dirs`: a zero
    /// `scanned` over an empty `read_dirs` means the scan never reached a
    /// file, not that the claims were gone.
    pub read_dirs: Vec<PathBuf>,
}

/// Holder prefixes whose suffix IS a harness session id, plus
/// `mux-placement:<pid>:<uuid>` whose TRAILING segment is one.
const SESSION_HOLDER_PREFIXES: [&str; 3] = ["target-session:", "review-session:", "session:"];

/// Does this record belong to the stopped holder?
///
/// A handover claim records the SPAWNER's session through an ambient pid
/// (measured 2026-09-11: `node:x-9c91` held by `spawn-handover:target-x-9c91-…`
/// with the blueprint session's id and a dead pid). A session-id match alone
/// would hand a freshly spawned worker's node to the next spawn when the
/// spawner is stopped, so `spawn-handover:<other>` never releases unless
/// `<other>` IS the stopped name.
fn record_belongs_to_stopped(rec: &ClaimRecord, target: &StoppedHolder) -> bool {
    if let Some(worker) = rec.holder.strip_prefix("spawn-handover:") {
        return worker == target.name;
    }
    let Some(session) = target
        .harness_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    if rec.session_id.as_deref() == Some(session) {
        return true;
    }
    for prefix in SESSION_HOLDER_PREFIXES {
        if let Some(rest) = rec.holder.strip_prefix(prefix) {
            if rest == session {
                return true;
            }
        }
    }
    if let Some(rest) = rec.holder.strip_prefix("mux-placement:") {
        if let Some(uuid) = rest.rsplit(':').next() {
            if uuid == session {
                return true;
            }
        }
    }
    false
}

/// Holder-bound release at ONE verbatim claims directory (the per-dir twin of
/// [`release`], which resolves `<root>/.fno/claims` and so cannot reach a
/// spaces-era `<space>/claims` layout): re-read, require the holder to still
/// match, remove, emit `claim_released`.
fn release_stopped_at(
    dir: &Path,
    rec: &ClaimRecord,
    events_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = dir.join(format!("{}.lock", encode_key(&rec.key)));
    let existing = match read_claim_file(&path) {
        Ok(existing) => existing,
        Err(ReadError::GoneAway) => return Err("claim already gone".into()),
        Err(ReadError::Corrupted(error)) => return Err(error),
    };
    if existing.holder != rec.holder {
        return Err("holder changed since the scan".into());
    }
    let duration_ms = (now_ms() - existing.acquired_at).max(0);
    std::fs::remove_file(&path).map_err(|error| error.to_string())?;
    let mut data = common_event_data(&existing);
    data.insert("duration_held_ms".into(), Value::Number(duration_ms.into()));
    emit_audit_event(events_dir, "claim_released", data);
    Ok(path)
}

/// The stopped worker's harness session id, read from the registry row a
/// stop leaves behind. `None` when no row names the worker or the row
/// carries no session; the release then matches holder shapes only.
pub fn session_for_name(registry_path: &Path, name: &str) -> Option<String> {
    let rows = crate::client_verbs::read_registry_entries(registry_path).ok()?;
    rows.iter()
        .filter(|row| row.get("name").and_then(Value::as_str) == Some(name))
        .find_map(|row| {
            row.get("harness_session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

/// Release every claim the stopped holder keeps, across VERBATIM claims
/// directories (the global dir and the worker's own space dir). One
/// implementation serves both the daemon (direct call) and the Python leg
/// (through the `release-stopped` op), so no parity guard is needed.
///
/// Selection, the pid guard, and the receipt: a record releases only when it
/// belongs to the stopped holder AND its pid is provably gone (or absent).
/// A live pid, an unreadable pid, and another machine all keep the claim
/// with the reading that kept it.
pub fn release_for_stopped_session(
    target: &StoppedHolder,
    dirs: &[PathBuf],
    events_dir: Option<&Path>,
) -> Result<StopReleaseReceipt, String> {
    let (records, read_dirs) = list_in_result(dirs, None, true)?;
    let mut receipt = StopReleaseReceipt {
        scanned: records.len(),
        dirs: dirs.to_vec(),
        read_dirs,
        ..Default::default()
    };
    for rec in &records {
        if !record_belongs_to_stopped(rec, target) {
            continue;
        }
        let kept_observed = match rec.pid {
            // No pid to probe: the holder identity itself is the release proof.
            None => None,
            Some(pid) if !is_same_machine(&rec.host, rec.machine_id.as_deref()) => {
                Some(format!("off-host {}", rec.host))
            }
            Some(pid) => match probe_pid(pid) {
                PidProbe::Absent => None,
                PidProbe::Refused => Some(format!("pid {pid} access-denied")),
                PidProbe::Created(_) => Some(format!("pid {pid} present")),
            },
        };
        if let Some(observed) = kept_observed {
            receipt.kept.push(StopKeptClaim {
                key: rec.key.clone(),
                holder: rec.holder.clone(),
                observed,
            });
            continue;
        }
        let mut released_path = None;
        for dir in dirs {
            match release_stopped_at(dir, rec, events_dir) {
                Ok(path) => {
                    released_path = Some(path);
                    break;
                }
                Err(_) => continue,
            }
        }
        if let Some(path) = released_path {
            receipt.released.push(StopReleasedClaim {
                key: rec.key.clone(),
                holder: rec.holder.clone(),
                path,
            });
        }
        // No dir still held it: the claim vanished between the scan and the
        // release (a concurrent holder release), so it appears in neither
        // list - the same accounting as release()'s GoneAway success.
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{hostname, machine_id, ClaimRecord, SCHEMA_VERSION};
    use tempfile::TempDir;

    // A pid the OS does not report as alive (copy of the claims.rs test helper).
    fn dead_pid() -> u32 {
        let mut candidate = 999_999u32;
        while unsafe { libc::kill(candidate as i32, 0) } == 0 {
            candidate += 1;
        }
        candidate
    }

    fn read_events_from(dir: &Path) -> Vec<Value> {
        let mut text = std::fs::read_to_string(dir.join(".fno/events.jsonl")).unwrap_or_default();
        text.push_str(
            &std::fs::read_to_string(dir.join(".fno/events.jsonl.ephemeral")).unwrap_or_default(),
        );
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn stopped_rec(
        key: &str,
        holder: &str,
        pid: Option<i32>,
        session: Option<&str>,
        provenance: &str,
    ) -> ClaimRecord {
        ClaimRecord {
            schema_version: SCHEMA_VERSION,
            key: key.into(),
            holder: holder.into(),
            acquired_at: now_ms(),
            pid,
            host: hostname(),
            pid_unavailable: false,
            expires_at: Some(now_ms() + 3_600_000),
            reason: None,
            harness: None,
            session_id: session.map(Into::into),
            pid_provenance: Some(provenance.into()),
            machine_id: Some(machine_id()),
            metadata: Default::default(),
        }
    }

    fn write_rec(dir: &Path, rec: &ClaimRecord) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.lock", encode_key(&rec.key))),
            serde_json::to_string(rec).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn release_for_stopped_session_releases_owned_dead_and_keeps_live() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        let session = "sess-9c91";
        // x-a: the stopped session's own node claim, pid gone -> released.
        write_rec(
            &claims_dir,
            &stopped_rec(
                "node:x-a",
                "target-session:w1",
                Some(dead_pid() as i32),
                Some(session),
                "session-prover",
            ),
        );
        // x-b: the handover claim the stopped worker itself spawned -> released.
        write_rec(
            &claims_dir,
            &stopped_rec(
                "node:x-b",
                "spawn-handover:w1",
                Some(dead_pid() as i32),
                None,
                "ambient",
            ),
        );
        // x-c: same session, but the pid is THIS live test process -> kept.
        write_rec(
            &claims_dir,
            &stopped_rec(
                "node:x-c",
                "target-session:w1",
                Some(std::process::id() as i32),
                Some(session),
                "session-prover",
            ),
        );
        // x-d: a different session's claim -> untouched.
        write_rec(
            &claims_dir,
            &stopped_rec(
                "node:x-d",
                "target-session:other",
                Some(dead_pid() as i32),
                Some("sess-other"),
                "session-prover",
            ),
        );
        // x-e: a DIFFERENT worker's handover carrying the spawner's session id
        // (the ambient-pid shape) -> untouched, or a stop of w1 would hand
        // w2's freshly spawned node to the next spawn.
        write_rec(
            &claims_dir,
            &stopped_rec(
                "node:x-e",
                "spawn-handover:w2",
                Some(dead_pid() as i32),
                Some(session),
                "ambient",
            ),
        );

        let target = StoppedHolder {
            name: "w1".into(),
            harness_session_id: Some(session.into()),
        };
        let receipt =
            release_for_stopped_session(&target, &[claims_dir.clone()], Some(td.path())).unwrap();

        assert_eq!(receipt.scanned, 5);
        let mut released: Vec<String> = receipt.released.iter().map(|r| r.key.clone()).collect();
        released.sort();
        assert_eq!(released, vec!["node:x-a", "node:x-b"]);
        assert!(!lockfile_of(&claims_dir, "node:x-a").exists());
        assert!(!lockfile_of(&claims_dir, "node:x-b").exists());
        assert_eq!(receipt.kept.len(), 1);
        assert_eq!(receipt.kept[0].key, "node:x-c");
        assert!(
            receipt.kept[0].observed.contains("present"),
            "kept reading names the live pid: {}",
            receipt.kept[0].observed
        );
        for untouched in ["node:x-d", "node:x-e"] {
            assert!(
                lockfile_of(&claims_dir, untouched).exists(),
                "{untouched} must stay on disk"
            );
            assert!(!receipt.released.iter().any(|r| r.key == untouched));
            assert!(!receipt.kept.iter().any(|r| r.key == untouched));
        }
        let events = read_events_from(td.path());
        let released_events = events
            .iter()
            .filter(|e| e["type"] == "claim_released")
            .count();
        assert_eq!(released_events, 2, "each release emits claim_released");
    }

    fn lockfile_of(dir: &Path, key: &str) -> PathBuf {
        dir.join(format!("{}.lock", encode_key(key)))
    }

    #[test]
    fn zero_scan_over_a_missing_dir_reads_nothing_and_says_so() {
        let td = TempDir::new().unwrap();
        let missing_a = td.path().join("missing-a");
        let missing_b = td.path().join("missing-b");
        let target = StoppedHolder {
            name: "w1".into(),
            harness_session_id: None,
        };
        let receipt = release_for_stopped_session(&target, &[missing_a, missing_b], None).unwrap();
        assert_eq!(receipt.read_dirs, Vec::<PathBuf>::new());
        assert_eq!(receipt.dirs.len(), 2);
        assert_eq!(receipt.scanned, 0);
    }

    #[test]
    fn zero_scan_over_present_empty_dirs_reads_both_and_says_so() {
        let td = TempDir::new().unwrap();
        let empty_a = td.path().join("empty-a");
        let empty_b = td.path().join("empty-b");
        std::fs::create_dir_all(&empty_a).unwrap();
        std::fs::create_dir_all(&empty_b).unwrap();
        let target = StoppedHolder {
            name: "w1".into(),
            harness_session_id: None,
        };
        let receipt = release_for_stopped_session(&target, &[empty_a, empty_b], None).unwrap();
        assert_eq!(receipt.read_dirs.len(), 2);
        assert_eq!(receipt.dirs.len(), 2);
        assert_eq!(receipt.scanned, 0);
    }

    #[test]
    fn session_for_name_reads_the_stopped_row() {
        let td = TempDir::new().unwrap();
        let home = crate::paths::AgentsHome::at(td.path());
        std::fs::write(
            home.registry_json(),
            r#"{"schema_version": 1, "agents": [
                {"name": "w1", "harness": "claude", "status": "idle",
                 "cwd": "/tmp", "log_path": "/tmp/l",
                 "harness_session_id": "sess-9c91", "pid": 1},
                {"name": "w2", "harness": "codex", "status": "idle",
                 "cwd": "/tmp", "log_path": "/tmp/m", "pid": 2}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            session_for_name(&home.registry_json(), "w1").as_deref(),
            Some("sess-9c91")
        );
        assert_eq!(session_for_name(&home.registry_json(), "w2"), None);
        assert_eq!(session_for_name(&home.registry_json(), "nobody"), None);
    }
}
