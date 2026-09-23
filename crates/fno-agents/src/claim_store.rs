//! Lockfile helpers shared by claim readers that need a repo-space listing.

use crate::claims::{self, ClaimRecord, ClaimState};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Read one repo-space claims directory using the same root resolution as a
/// rootless claim operation. `claims::list` intentionally reads the global
/// directory plus an explicitly supplied repository root; repo-space claims
/// live directly under `<space>/claims` and need this resolver.
pub fn list_repo_space(prefix: &str, include_stale: bool) -> Result<Vec<ClaimRecord>, String> {
    let key = format!("{prefix}probe");
    let directory = crate::claims_root::claims_dir(&key, None)?;
    claims::list_in(
        std::slice::from_ref(&directory),
        Some(prefix),
        include_stale,
    )
}

fn claims_dir(root: Option<&Path>) -> Result<PathBuf, String> {
    claims::claims_dir_for(root).ok_or_else(|| "claims root is unavailable".to_string())
}

fn archive_path(path: &Path) -> Result<PathBuf, String> {
    let archive = path
        .parent()
        .ok_or_else(|| "claim path has no parent".to_string())?
        .join(".expired");
    std::fs::create_dir_all(&archive).map_err(|error| error.to_string())?;
    let stamp = claims::now_ms();
    let name = path
        .file_name()
        .ok_or_else(|| "claim path has no filename".to_string())?
        .to_string_lossy();
    Ok(archive.join(format!("{name}.{stamp}")))
}

pub fn force_release(key: &str, reason: &str, root: Option<&Path>) -> Result<Value, String> {
    if key.is_empty() {
        return Err("key must be non-empty".to_string());
    }
    if reason.trim().is_empty() {
        return Err("reason must be non-empty for force-release".to_string());
    }
    let path = claims::claim_path(key, root)?;
    if !path.exists() {
        return Ok(json!({
            "key": key,
            "path": path,
            "archived": false,
            "force_released": false,
            "previous_holder": Value::Null,
        }));
    }
    let previous_holder = claims::read_claim_file(&path)
        .ok()
        .map(|record| record.holder);
    let destination = archive_path(&path)?;
    std::fs::rename(&path, &destination).map_err(|error| error.to_string())?;
    Ok(json!({
        "key": key,
        "path": path,
        "archived": true,
        "force_released": true,
        "previous_holder": previous_holder,
    }))
}

pub fn reap(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    let directory = claims_dir(root)?;
    let records = if directory.is_dir() {
        claims::list_in(std::slice::from_ref(&directory), None, true)?
    } else {
        Vec::new()
    };
    let mut reaped = 0usize;
    let mut would_reap = 0usize;
    let mut failures = Vec::new();
    for record in records {
        if claims::classify(&record, None) != ClaimState::Stale {
            continue;
        }
        would_reap += 1;
        if !apply {
            continue;
        }
        let path = claims::claim_path(&record.key, root)?;
        let destination = archive_path(&path)?;
        match std::fs::rename(&path, &destination) {
            Ok(()) if !path.exists() && destination.exists() => reaped += 1,
            Ok(()) => failures.push(format!("archive verification failed: {}", path.display())),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    Ok(json!({
        "apply": apply,
        "scanned": would_reap,
        "would_reap": would_reap,
        "reaped": reaped,
        "reap_failed": failures,
        "root": directory,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct ClaimsRootRestore(Option<std::ffi::OsString>);

    impl Drop for ClaimsRootRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
                None => std::env::remove_var("FNO_CLAIMS_ROOT"),
            }
        }
    }

    fn with_claims_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
        let _env_lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let restore = ClaimsRootRestore(std::env::var_os("FNO_CLAIMS_ROOT"));
        std::env::set_var("FNO_CLAIMS_ROOT", root);
        let result = f();
        drop(restore);
        result
    }

    #[test]
    fn repo_space_listing_reads_the_resolved_lockfile_directory() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "merge-slot:main";
            assert!(matches!(
                claims::acquire(
                    key,
                    "pr:17",
                    claims::AcquireOpts {
                        pid_unavailable: true,
                        ttl_ms: Some(60_000),
                        ..Default::default()
                    }
                ),
                claims::AcquireOutcome::Acquired(_)
            ));

            let records = list_repo_space("merge-slot:", false).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].key, key);
            assert_eq!(records[0].holder, "pr:17");
        });
    }
}
