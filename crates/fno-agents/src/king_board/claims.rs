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
/// question itself (the x-9958 ruling: a lease must never suppress the row -
/// x-caf7 held a fresh lease while deadlocked, and the x-db9c dispatch that
/// commissioned this fix minted its own handover claim on the very node being
/// fixed). A role row whose window lapsed with no worker taking over is the
/// stale row `stale_claim` exists to name.
///
/// An unreadable directory is an ERR, never an empty row list: empty means
/// "nobody holds anything", and a failed read must not say that (x-636f).
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

    fn handover_row(holder: &str, expires_in_ms: i64, key: &str) -> (String, String) {
        let now = crate::claims::now_ms();
        let expires_at = now + expires_in_ms;
        // Handwritten YAML on purpose: the lock file is the artifact the
        // scanner reads, so the fixture is the artifact the writer would
        // have produced, not a private constructor.
        let yaml = format!(
            "schema_version: 1\nkey: \"{decoded}\"\nholder: \"{holder}\"\nacquired_at: {now}\npid: 1\nhost: test-host\nexpires_at: {expires_at}\nreason: \"spawn handover window for {decoded}\"\n",
            decoded = key.replace("%3A", ":"),
        );
        (format!("{key}.lock"), yaml)
    }

    fn scan_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kb-claims-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
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
        let (name, yaml) = handover_row("spawn-handover:t-90fa-port", 900_000, "node%3Ax-90fa");
        std::fs::write(dir.join(name), yaml).expect("write handover claim");
        let (tname, tyaml) =
            handover_row("target-session:a6d2ce6a-1da0", 900_000, "node%3Ax-requeue");
        std::fs::write(dir.join(tname), tyaml).expect("write requeue claim");
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
        let (name, yaml) = handover_row("spawn-handover:t-90fa-port", -1, "node%3Ax-90fa");
        std::fs::write(dir.join(name), yaml).expect("write expired handover claim");
        let rows = read_claims_in(&[dir.clone()]).rows();
        assert_eq!(rows.len(), 1, "expired handover must stay: {rows:?}");
        assert_eq!(rows[0]["state"], "stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_dir_reads_unreadable_not_empty() {
        // x-636f: the scan's old `read_dir` failure returned zero rows, and
        // zero rows means nobody holds anything. The read must say it failed.
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads through mode 000; the assertion cannot fire
        }
        let dir = scan_dir("mode000");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let read = read_claims_in(&[dir.clone()]);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("chmod restore");
        assert!(!read.is_ok(), "mode-000 dir must not read ok: {read:?}");
        let error = read.error.expect("error names the fault");
        assert!(
            error.contains(&dir.display().to_string()),
            "error names the unreadable dir: {error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
