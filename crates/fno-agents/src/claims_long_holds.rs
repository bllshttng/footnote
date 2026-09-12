//! Long single-flight holds (x-9c91 change 4): the rows `fno agents top`
//! shows for `flight:` holds older than the measured reconcile budget. One
//! implementation serves the Python `top` render through the `claim
//! long-holds` op, so the per-row pid probe and sidecar count live beside
//! the claims reader they depend on.
//!
//! The holder annotation is the session-aware classification (x-63f9): claim
//! basis outranks the pid, so an ambient pid is never printed as the holder
//! verdict. An absent pid prints on every row with the flight gate's own
//! consequence - the gate reclaims a pid-absent hold on the next acquire -
//! and an expired lease renders as its own axis, never a liveness verdict.

use crate::claims::{
    classify_with_basis_and_exclusivity, encode_key, is_expired, is_same_machine, list_in_result,
    now_ms, probe_pid, ClaimRecord, PidProbe, SessionWitness,
};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Twelve-minute reconcile runs are measured, per the FLIGHT_TTL_MS doc in
/// flight_gate.rs; a hold older than this is a long hold.
pub const DEFAULT_MIN_HOLD_S: i64 = 12 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct LongHoldRow {
    pub key: String,
    pub holder: String,
    pub pid: Option<i32>,
    pub pid_provenance: Option<String>,
    /// What the probe observed: `present`, `absent`, `unreadable` (no pid to
    /// probe, or the probe was denied), or `off-host` (the claim names another
    /// machine, so a local probe would answer a different question).
    pub pid_observed: String,
    /// Holder verdict from the same classifier `claim status` uses, so a
    /// session-bearing hold reads live on session evidence even when its
    /// ambient pid is gone.
    pub state: String,
    pub basis: String,
    /// Lease axis, kept separate from `state`: an expired lease on a live
    /// holder is a renewal question, never a death verdict.
    pub expired: bool,
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

/// Compact floored age, matching `top._fmt_age`: 45s / 12m / 3h.
fn fmt_age_s(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

/// The text block `top` prints after its lanes; silent when nothing is over
/// the budget. Rendered here so the rows and their presentation ship as one
/// projection.
fn render_lines(rows: &[LongHoldRow], min_hold_s: i64) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("single-flight holds over {}m:", min_hold_s / 60)];
    for row in rows {
        let mut line = format!(
            "  {}  holder {}  {} ({})  held {}  requests {}",
            row.key,
            row.holder,
            row.state,
            row.basis,
            fmt_age_s(row.held_s),
            row.requests
        );
        // The pid is holder evidence only when the record proves it. An
        // absent pid is printed on every row with its gate consequence: the
        // flight gate reclaims a pid-absent hold on the next acquire
        // (flight_gate.rs HeldByOther), whatever the session-healed state
        // says, so top never hides the lock's real future behind the basis.
        let pid = match row.pid {
            Some(pid) => pid.to_string(),
            None => "None".to_string(),
        };
        if row.pid_provenance.as_deref() == Some("session-prover") {
            line.push_str(&format!("  pid {pid} ({})", row.pid_observed));
        } else if row.pid_observed == "absent" {
            line.push_str(&format!(
                "  pid {pid} absent, gate reclaims on next acquire"
            ));
        }
        if row.expired && row.state != "stale" {
            line.push_str("  lease expired");
        }
        lines.push(line);
    }
    lines
}

/// `flight:` holds older than `min_hold_s` across the given claims
/// directories, longest hold first, with the `top` text block.
pub fn long_holds(dirs: &[PathBuf], min_hold_s: i64) -> Result<Value, String> {
    let (witness, _) = crate::claim_verbs::default_session_witness();
    let witness: SessionWitness = &witness;
    long_holds_with(dirs, min_hold_s, Some(witness))
}

/// `long_holds` with the session witness injectable, mirroring
/// `classify_with_basis_and_exclusivity`'s seam for tests.
pub(crate) fn long_holds_with(
    dirs: &[PathBuf],
    min_hold_s: i64,
    witness: Option<SessionWitness<'_>>,
) -> Result<Value, String> {
    let (records, _) = list_in_result(dirs, Some("flight:"), true)?;
    let now = now_ms();
    let mut rows: Vec<LongHoldRow> = Vec::new();
    for rec in &records {
        let held_s = (now - rec.acquired_at).max(0) / 1000;
        if held_s <= min_hold_s {
            continue;
        }
        let requests = record_dir(dirs, rec)
            .map(|dir| sidecar_requests(dir, &rec.key))
            .unwrap_or(0);
        let (state, hold_basis) = classify_with_basis_and_exclusivity(
            rec,
            Some(now),
            &|pid| probe_pid(pid),
            None,
            witness,
        );
        rows.push(LongHoldRow {
            key: rec.key.clone(),
            holder: rec.holder.clone(),
            pid: rec.pid,
            pid_provenance: rec.pid_provenance.clone(),
            pid_observed: pid_observed(rec),
            state: state.as_str().into(),
            basis: hold_basis.into(),
            expired: is_expired(rec, now),
            held_s,
            requests,
        });
    }
    rows.sort_by(|a, b| b.held_s.cmp(&a.held_s));
    let lines = render_lines(&rows, min_hold_s);
    serde_json::to_value(&rows)
        .map(|rows_json| serde_json::json!({ "rows": rows_json, "lines": lines }))
        .map_err(|error| error.to_string())
}

/// `fno-agents claim long-holds [--min-hold-s <s>] --claims-dir <dir>` (the
/// dir flag repeatable). Prints one JSON object `{"rows":[...],"lines":[...]}`
/// (rows plus the ready-to-print `top` text block); exit 0, or 3 when a
/// claims directory is unreadable.
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
    match long_holds(&dirs, min_hold_s) {
        Ok(payload) => {
            println!("{payload}");
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
    use crate::claims::{basis, hostname, machine_id, SessionLiveness, SCHEMA_VERSION};
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

        let payload = long_holds(&[claims_dir.clone()], DEFAULT_MIN_HOLD_S).unwrap();
        let rows = payload["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "only the 13m flight hold is a row");
        let row = &rows[0];
        assert_eq!(row["key"], "flight:abc");
        assert_eq!(row["holder"], "single-flight:x");
        assert_eq!(row["pid_observed"], "absent");
        assert_eq!(
            row["state"], "stale",
            "ambient pid, dead: the pid IS the verdict here"
        );
        assert_eq!(row["basis"], "pid-absent");
        assert_eq!(row["expired"], false);
        assert!(row["held_s"].as_i64().unwrap() >= 780);
        assert_eq!(row["requests"], 3, "blank trailing line is not a request");
        let lines = payload["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2, "caption plus one row line");
        assert_eq!(lines[0], "single-flight holds over 12m:");
        let rendered = lines[1].as_str().unwrap();
        assert!(rendered.contains("flight:abc"), "{rendered}");
        assert!(rendered.contains("stale (pid-absent)"), "{rendered}");
        assert!(rendered.contains("requests 3"), "{rendered}");
        assert!(
            rendered.contains("gate reclaims on next acquire"),
            "the gate consequence prints on every absent pid: {rendered}"
        );

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
        let rows = long_holds(&[claims_dir], DEFAULT_MIN_HOLD_S).unwrap()["rows"]
            .as_array()
            .unwrap()
            .clone();
        let keys: Vec<&str> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
        assert_eq!(keys, vec!["flight:older", "flight:abc"]);
    }

    #[test]
    fn live_session_heals_the_annotation_and_names_the_pid_disagreement() {
        // The x-63f9 specimen: expired lease, ambient pid gone, holder
        // session demonstrably alive. The annotation must read live on the
        // session witness, name the absent pid as ambient (never as the
        // verdict), and keep the lapsed lease on its own axis.
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        let mut rec = flight_rec(
            "flight:recon",
            "single-flight:5362:6e1e1699",
            Some(dead_pid() as i32),
            60,
        );
        rec.session_id = Some("dfd6631f".into());
        rec.pid_provenance = Some("ambient".into());
        rec.expires_at = Some(now_ms() - 60_000);
        write_rec(&claims_dir, &rec);

        let witness: SessionWitness = &|_| SessionLiveness::Live(basis::TRANSCRIPT_LIVE);
        let payload = long_holds_with(&[claims_dir], DEFAULT_MIN_HOLD_S, Some(witness)).unwrap();
        let row = &payload["rows"].as_array().unwrap()[0];
        assert_eq!(row["state"], "live");
        assert_eq!(row["basis"], "transcript-live");
        assert_eq!(row["expired"], true);
        let rendered = payload["lines"].as_array().unwrap()[1].as_str().unwrap();
        assert!(rendered.contains("live (transcript-live)"), "{rendered}");
        assert!(
            rendered.contains("absent, gate reclaims on next acquire"),
            "{rendered}"
        );
        assert!(rendered.contains("lease expired"), "{rendered}");
        let dead = dead_pid();
        assert!(
            !rendered.contains(&format!("pid {dead} (absent)")),
            "{rendered}"
        );
    }

    #[test]
    fn proven_pid_still_prints_its_probe() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        let mut rec = flight_rec(
            "flight:proven",
            "single-flight:p",
            Some(dead_pid() as i32),
            15,
        );
        rec.pid_provenance = Some("session-prover".into());
        write_rec(&claims_dir, &rec);

        let payload = long_holds(&[claims_dir], DEFAULT_MIN_HOLD_S).unwrap();
        let row = &payload["rows"].as_array().unwrap()[0];
        assert_eq!(row["state"], "stale");
        assert_eq!(row["basis"], "pid-absent");
        let rendered = payload["lines"].as_array().unwrap()[1].as_str().unwrap();
        let dead = dead_pid();
        assert!(
            rendered.contains(&format!("pid {dead} (absent)")),
            "{rendered}"
        );
    }

    #[test]
    fn expired_lease_on_unresolved_holder_reads_suspect_not_dead() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        let mut rec = flight_rec(
            "flight:gray",
            "single-flight:g",
            Some(dead_pid() as i32),
            60,
        );
        rec.session_id = Some("sess-gray".into());
        rec.expires_at = Some(now_ms() - 60_000);
        write_rec(&claims_dir, &rec);

        let witness: SessionWitness = &|_| SessionLiveness::Unresolved;
        let payload = long_holds_with(&[claims_dir], DEFAULT_MIN_HOLD_S, Some(witness)).unwrap();
        let row = &payload["rows"].as_array().unwrap()[0];
        assert_eq!(row["state"], "suspect", "inside the grace window");
        assert_eq!(row["basis"], "ttl-expired-unresolved");
        assert_eq!(row["expired"], true);
        let rendered = payload["lines"].as_array().unwrap()[1].as_str().unwrap();
        assert!(
            rendered.contains("suspect (ttl-expired-unresolved)"),
            "{rendered}"
        );
        assert!(rendered.contains("lease expired"), "{rendered}");
        assert!(!rendered.contains("lease expired, holder"), "{rendered}");
    }

    #[test]
    fn ambient_present_pid_prints_no_pid_segment() {
        // The pid rule's other arm, pinned at the renderer so no live-pid
        // probe is involved: a live ambient pid is ordinary context, not
        // holder evidence, so the line carries no pid segment at all.
        let row = LongHoldRow {
            key: "flight:direct".into(),
            holder: "single-flight:s".into(),
            pid: Some(1),
            pid_provenance: None,
            pid_observed: "present".into(),
            state: "live".into(),
            basis: "live".into(),
            expired: false,
            held_s: 900,
            requests: 0,
        };
        let lines = render_lines(&[row], DEFAULT_MIN_HOLD_S);
        assert!(lines[1].contains("live (live)"), "{}", lines[1]);
        assert!(
            !lines[1].contains(" pid "),
            "ambient pid never prints: {}",
            lines[1]
        );
    }

    #[test]
    fn off_host_hold_never_probes_a_foreign_pid() {
        let td = TempDir::new().unwrap();
        let claims_dir = td.path().join("claims");
        let mut rec = flight_rec("flight:remote", "single-flight:r", Some(1), 15);
        rec.host = "some-other-host".into();
        rec.machine_id = Some("other-machine".into());
        write_rec(&claims_dir, &rec);

        let rows = long_holds(&[claims_dir], DEFAULT_MIN_HOLD_S).unwrap()["rows"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["pid_observed"], "off-host");
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
