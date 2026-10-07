//! The merged both-roots claim scan (claims.cli._merge_claims_across_roots).
use super::SourceRead;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Claims: the merged both-roots scan (claims.cli._merge_claims_across_roots)
// ---------------------------------------------------------------------------

/// One root's live + dead claim rows (core._list_claims_impl with
/// include_stale=true): every `.lock` file, classified, dead states kept.
///
/// A role-prefixed row (a launch window, a requeue reservation) STAYS in the
/// scan: it is workflow state, but the board's probe layer is what decides
/// whether a worker stands behind it, and a lease must never answer that
/// question itself (the ruling: a lease must never suppress the row -
/// held a fresh lease while deadlocked, and the dispatch that
/// commissioned this fix minted its own handover claim on the very node being
/// fixed). A role row whose window lapsed with no worker taking over is the
/// stale row `stale_claim` exists to name.
///
/// An unreadable directory is an ERR, never an empty row list: empty means
/// "nobody holds anything", and a failed read must not say that.
pub(crate) fn read_claims_in(dirs: &[PathBuf]) -> SourceRead {
    let records = match crate::claims::list_in(dirs, Some("node:"), true) {
        Ok(records) => records,
        Err(e) => return SourceRead::err(format!("claims unreadable: {e}")),
    };
    SourceRead::ok(Value::Array(
        records
            .iter()
            .map(|rec| {
                json!({
                    "key": rec.key,
                    "state": crate::claims::classify(rec, None).as_str(),
                    "holder": rec.holder,
                    "host": rec.host,
                    "pid": rec.pid,
                    "session_id": rec.session_id,
                    // Epoch MILLISECONDS; the distress staleness compare
                    // divides by 1000 before reading it against a row ts.
                    "acquired_at": rec.acquired_at,
                })
            })
            .collect(),
    ))
}

/// Both roots (the global claims root, then the canonical checkout's own),
/// deduped by canonicalized path, read through [`read_claims_in`].
pub(crate) fn read_claims(cwd: &Path) -> SourceRead {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let push = |dir: Option<PathBuf>,
                seen: &mut std::collections::HashSet<PathBuf>,
                dirs: &mut Vec<PathBuf>| {
        if let Some(d) = dir {
            let resolved = d.canonicalize().unwrap_or_else(|_| d.clone());
            if seen.insert(resolved) {
                dirs.push(d);
            }
        }
    };
    // The global root (claims_root_for("node:") = FNO_CLAIMS_ROOT or $HOME),
    // then the canonical checkout's own root; dedup when they are the same.
    push(crate::claims::claims_dir_for(None), &mut seen, &mut dirs);
    let canonical = crate::paths::canonical_repo_root(cwd);
    push(
        canonical.and_then(|c| crate::claims::claims_dir_for(Some(&c))),
        &mut seen,
        &mut dirs,
    );
    if dirs.is_empty() {
        return SourceRead::err("agents claim list: no claims root resolves");
    }
    read_claims_in(&dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn claim(key: &str, holder: &str, expires_at: i64) -> crate::claims::ClaimRecord {
        let now = crate::claims::now_ms();
        crate::claims::ClaimRecord {
            schema_version: 1,
            key: key.into(),
            holder: holder.into(),
            acquired_at: now,
            pid: Some(1),
            host: "test-host".into(),
            pid_unavailable: false,
            expires_at: Some(expires_at),
            reason: Some(format!("spawn handover window for {key}")),
            harness: None,
            session_id: None,
            pid_provenance: None,
            machine_id: None,
            metadata: Default::default(),
        }
    }

    fn handover_row(
        holder: &str,
        expires_in_ms: i64,
        key: &str,
    ) -> (String, crate::claims::ClaimRecord) {
        let rec = claim(key, holder, crate::claims::now_ms() + expires_in_ms);
        (format!("{}.lock", crate::claims::encode_key(key)), rec)
    }

    /// A claims dir whose state root (`graph.db` home) is private to the test.
    fn scan_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("kb-claims-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        root.join("claims")
    }

    #[test]
    fn a_live_handover_row_stays_in_the_scan() {
        // The regression guard task 4 exists to pin: a live launch-window
        // lease is a row the board probes, never one the scan drops. The
        // lease-keyed skip this test retires removed the row
        // pre-classification, so an in_progress node in its launch window
        // read driver-none and landed in unheld_progress - the exact
        // silence the scan-level skip manufactured.
        let dir = scan_dir("live");
        let (name, rec) = handover_row("spawn-handover:t-90fa-port", 900_000, "node:x-90fa");
        crate::claim_store::seed_at_path(&dir.join(name), &rec);
        let (tname, trec) = handover_row("target-session:a6d2ce6a-1da0", 900_000, "node:x-requeue");
        crate::claim_store::seed_at_path(&dir.join(tname), &trec);
        let rows = read_claims_in(&[dir.clone()]).rows();
        assert_eq!(rows.len(), 2, "role rows stay in scope: {rows:?}");
        assert!(
            rows.iter()
                .all(|r| r["state"] == "live" || r["state"] == "suspect"),
            "an unexpired window is never stale: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_handover_row_stays_in_scope() {
        let dir = scan_dir("exp");
        let (name, rec) = handover_row("spawn-handover:t-90fa-port", -1, "node:x-90fa");
        crate::claim_store::seed_at_path(&dir.join(name), &rec);
        let rows = read_claims_in(&[dir.clone()]).rows();
        assert_eq!(rows.len(), 1, "expired handover must stay: {rows:?}");
        assert_eq!(rows[0]["state"], "stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_claim_row_names_its_holder_session_and_when_it_took_the_claim() {
        // AC1: the projection stamps session_id and acquired_at, the two
        // keys claim_session_by_node (org_board.rs) and the distress
        // staleness compare read. A pre-change lockfile carries neither;
        // its row stays with session_id null, never an error or a drop.
        let dir = scan_dir("session-id");
        let now = crate::claims::now_ms();
        let mut named = claim("node:x-cccc", "target-session:a6d2ce6a-1da0", now + 900_000);
        named.acquired_at = now;
        named.session_id = Some("target-session:a6d2ce6a-1da0".into());
        crate::claim_store::seed_at_path(&dir.join("node%3Ax-cccc.lock"), &named);
        let (name, pre_change) =
            handover_row("spawn-handover:t-90fa-port", 900_000, "node:x-requeue");
        crate::claim_store::seed_at_path(&dir.join(name), &pre_change);
        let rows = read_claims_in(&[dir.clone()]).rows();
        assert_eq!(rows.len(), 2, "both rows stay in scope: {rows:?}");
        let named = rows
            .iter()
            .find(|r| r["key"] == "node:x-cccc")
            .expect("session-carrying row present");
        assert_eq!(named["session_id"], "target-session:a6d2ce6a-1da0");
        assert_eq!(named["acquired_at"], now, "epoch milliseconds, unchanged");
        let pre = rows
            .iter()
            .find(|r| r["key"] == "node:x-requeue")
            .expect("pre-change row present");
        assert!(
            pre["session_id"].is_null(),
            "absent session_id reads null, not a parse error: {pre:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_dir_reads_unreadable_not_empty() {
        // the scan's old `read_dir` failure returned zero rows, and
        // zero rows means nobody holds anything. The read must say it failed.
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads through mode 000; the assertion cannot fire
        }
        let dir = scan_dir("mode000");
        let root = dir.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let read = read_claims_in(&[dir.clone()]);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
            .expect("chmod restore");
        assert!(!read.is_ok(), "mode-000 dir must not read ok: {read:?}");
        let error = read.error.expect("error names the fault");
        assert!(
            error.contains(&root.display().to_string()),
            "error names the unreadable state root: {error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
