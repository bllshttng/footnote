//! Long single-flight holds (x-9c91 change 4): the rows `fno agents top`
//! shows for `flight:` holds older than the measured reconcile budget. One
//! implementation serves the Python `top` render through the `claim
//! long-holds` op, so the per-row pid probe and sidecar count live beside
//! the claims reader they depend on.

use crate::claims::{
    encode_key, is_same_machine, list_in_result, now_ms, probe_pid, ClaimRecord, PidProbe,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Twelve-minute reconcile runs are measured, per the FLIGHT_TTL_MS doc in
/// flight_gate.rs; a hold older than this is a long hold.
pub const DEFAULT_MIN_HOLD_S: i64 = 12 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct LongHoldRow {
    pub key: String,
    pub holder: String,
    pub pid: Option<i32>,
    /// What the probe observed: `present`, `absent`, `unreadable` (no pid to
    /// probe, or the probe was denied), or `off-host` (the claim names another
    /// machine, so a local probe would answer a different question).
    pub pid_observed: String,
    pub held_s: i64,
    /// Lines in the flight gate's `.held-requests` sidecar (per-holder count).
    pub requests: u64,
}

fn sidecar_requests(cdir: &Path, key: &str) -> u64 {
    std::fs::read_to_string(cdir.join(format!("{}.held-requests", encode_key(key))))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count() as u64)
        .unwrap_or(0)
}

fn pid_observed(rec: &ClaimRecord) -> String {
    let Some(pid) = rec.pid else {
        return "unreadable".into();
    };
    if !is_same_machine(&rec.host, rec.machine_id.as_deref()) {
        return "off-host".into();
    }
    match probe_pid(pid) {
        PidProbe::Created(_) => "present".into(),
        PidProbe::Refused => "unreadable".into(),
        PidProbe::Absent => "absent".into(),
    }
}

/// Where the record's lockfile sits: the sidecar the gate wrote belongs to
/// that directory, not to whichever dir is listed first.
fn record_dir<'a>(dirs: &'a [PathBuf], rec: &ClaimRecord) -> Option<&'a Path> {
    let name = format!("{}.lock", encode_key(&rec.key));
    dirs.iter()
        .find(|dir| dir.join(&name).exists())
        .map(PathBuf::as_path)
}

/// `flight:` holds older than `min_hold_s` across the given claims
/// directories, longest hold first.
pub fn long_hold_rows(dirs: &[PathBuf], min_hold_s: i64) -> Result<Vec<LongHoldRow>, String> {
    let records = list_in_result(dirs, Some("flight:"), true)?;
    let now = now_ms();
    let mut rows = Vec::new();
    for rec in &records {
        let held_s = (now - rec.acquired_at).max(0) / 1000;
        if held_s <= min_hold_s {
            continue;
        }
        let requests = record_dir(dirs, rec)
            .map(|dir| sidecar_requests(dir, &rec.key))
            .unwrap_or(0);
        rows.push(LongHoldRow {
            key: rec.key.clone(),
            holder: rec.holder.clone(),
            pid: rec.pid,
            pid_observed: pid_observed(rec),
            held_s,
            requests,
        });
    }
    rows.sort_by(|a, b| b.held_s.cmp(&a.held_s));
    Ok(rows)
}

/// `fno-agents claim long-holds [--min-hold-s <s>] --claims-dir <dir>` (the
/// dir flag repeatable). Prints one JSON object `{"rows":[...]}`; exit 0, or
/// 3 when a claims directory is unreadable.
pub fn run_claim_long_holds(args: &[String]) -> i32 {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut min_hold_s = DEFAULT_MIN_HOLD_S;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--min-hold-s" => match it.next().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) => min_hold_s = v,
                None => {
                    eprintln!("fno-agents: claim long-holds: --min-hold-s requires a value");
                    return 2;
                }
            },
            "--claims-dir" => match it.next() {
                Some(v) => dirs.push(PathBuf::from(v)),
                None => {
                    eprintln!("fno-agents: claim long-holds: --claims-dir requires a value");
                    return 2;
                }
            },
            other => {
                eprintln!("fno-agents: claim long-holds: unknown flag {other}");
                return 2;
            }
        }
    }
    if dirs.is_empty() {
        eprintln!("fno-agents: claim long-holds requires --claims-dir");
        return 2;
    }
    match long_hold_rows(&dirs, min_hold_s) {
        Ok(rows) => {
            println!("{}", serde_json::json!({ "rows": rows }));
            0
        }
        Err(error) => {
            eprintln!("fno-agents: claim long-holds: {error}");
            3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{hostname, machine_id, SCHEMA_VERSION};
    use serde_json::Value;
    use tempfile::TempDir;

    fn dead_pid() -> u32 {
        let mut candidate = 999_999u32;
        while unsafe { libc::kill(candidate as i32, 0) } == 0 {
            candidate += 1;
        }
        candidate
    }

    fn flight_rec(key: &str, holder: &str, pid: Option<i32>, acquired_min_ago: i64) -> ClaimRecord {
        ClaimRecord {
            schema_version: SCHEMA_VERSION,
            key: key.into(),
            holder: holder.into(),
            acquired_at: now_ms() - acquired_min_ago * 60_000,
            pid,
            host: hostname(),
            pid_unavailable: false,
            expires_at: None,
            reason: None,
            harness: None,
            session_id: None,
            pid_provenance: None,
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
    fn long_holds_row_shape_threshold_and_order() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        // 13m old, pid gone: the long-hold row.
        write_rec(
            &claims_dir,
            &flight_rec("flight:abc", "single-flight:x", Some(dead_pid() as i32), 13),
        );
        // 5m old: inside the budget, never a row.
        write_rec(
            &claims_dir,
            &flight_rec(
                "flight:fresh",
                "single-flight:y",
                Some(dead_pid() as i32),
                5,
            ),
        );
        // A non-flight claim: not this op's scan.
        write_rec(
            &claims_dir,
            &flight_rec("node:z", "target-session:w", Some(dead_pid() as i32), 90),
        );
        std::fs::write(
            claims_dir.join(format!("{}.held-requests", encode_key("flight:abc"))),
            "1 1\n2 2\n3 3\n\n",
        )
        .unwrap();

        let rows = long_hold_rows(&[claims_dir.clone()], DEFAULT_MIN_HOLD_S).unwrap();
        assert_eq!(rows.len(), 1, "only the 13m flight hold is a row");
        let row = &rows[0];
        assert_eq!(row.key, "flight:abc");
        assert_eq!(row.holder, "single-flight:x");
        assert_eq!(row.pid_observed, "absent");
        assert!(row.held_s >= 780, "held_s {}", row.held_s);
        assert_eq!(row.requests, 3, "blank trailing line is not a request");

        // Longest first when two qualify.
        write_rec(
            &claims_dir,
            &flight_rec(
                "flight:older",
                "single-flight:z",
                Some(dead_pid() as i32),
                20,
            ),
        );
        let rows = long_hold_rows(&[claims_dir], DEFAULT_MIN_HOLD_S).unwrap();
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["flight:older", "flight:abc"]);
    }

    #[test]
    fn off_host_hold_never_probes_a_foreign_pid() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        let mut rec = flight_rec("flight:remote", "single-flight:r", Some(1), 15);
        rec.host = "some-other-host".into();
        rec.machine_id = Some("other-machine".into());
        write_rec(&claims_dir, &rec);

        let rows = long_hold_rows(&[claims_dir], DEFAULT_MIN_HOLD_S).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid_observed, "off-host");
    }

    #[test]
    fn op_prints_json_and_refuses_missing_dir_flag() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        write_rec(
            &claims_dir,
            &flight_rec("flight:op", "single-flight:o", None, 13),
        );
        let code = run_claim_long_holds(&[
            "--min-hold-s".into(),
            "720".into(),
            "--claims-dir".into(),
            claims_dir.to_string_lossy().into_owned(),
        ]);
        assert_eq!(code, 0);
        let code = run_claim_long_holds(&["--min-hold-s".into(), "720".into()]);
        assert_eq!(code, 2, "no --claims-dir is a usage error");
    }
}
