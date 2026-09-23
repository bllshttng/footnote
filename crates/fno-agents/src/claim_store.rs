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
    claims::with_recovery_lock(&path, || {
        if !path.exists() {
            return Ok(json!({
                "key": key,
                "path": path.clone(),
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
            "path": path.clone(),
            "archived": true,
            "force_released": true,
            "previous_holder": previous_holder,
        }))
    })
}

fn reap_one(path: &Path, expected: &ClaimRecord) -> Result<bool, String> {
    reap_one_with_session_witness(path, expected, None)
}

fn reap_one_with_session_witness(
    path: &Path,
    expected: &ClaimRecord,
    session_witness: Option<claims::SessionWitness<'_>>,
) -> Result<bool, String> {
    claims::with_recovery_lock(path, || {
        let current = match claims::read_claim_file(path) {
            Ok(record) => record,
            Err(claims::ReadError::GoneAway) => return Ok(false),
            Err(claims::ReadError::Corrupted(error)) => {
                return Err(format!("{}: corrupted claim: {error}", path.display()))
            }
        };
        if &current != expected {
            return Ok(false);
        }
        if claims::classify_with_session_witness(&current, session_witness) != ClaimState::Stale {
            return Ok(false);
        }
        let destination = archive_path(path)?;
        std::fs::rename(path, &destination).map_err(|error| error.to_string())?;
        let archived = claims::read_claim_file(&destination).map_err(|error| {
            format!(
                "archive verification failed: {}: {error:?}",
                destination.display()
            )
        })?;
        if archived.key != current.key
            || archived.holder != current.holder
            || archived.acquired_at != current.acquired_at
        {
            return Err(format!(
                "archive verification failed: {} changed during reap",
                path.display()
            ));
        }
        Ok(true)
    })
}

pub fn reap(root: Option<&Path>, apply: bool) -> Result<Value, String> {
    reap_with_session_witness(root, apply, None, None)
}

pub(crate) fn reap_with_session_witness(
    root: Option<&Path>,
    apply: bool,
    session_witness: Option<claims::SessionWitness<'_>>,
    recheck_witness: Option<claims::SessionWitness<'_>>,
) -> Result<Value, String> {
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
        if claims::classify_with_session_witness(&record, session_witness) != ClaimState::Stale {
            continue;
        }
        would_reap += 1;
        if !apply {
            continue;
        }
        let path = claims::claim_path(&record.key, root)?;
        match reap_one_with_session_witness(&path, &record, recheck_witness) {
            Ok(true) => reaped += 1,
            Ok(false) => {}
            Err(error) => failures.push(error),
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
    use std::thread;
    use std::time::Duration;
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

    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn live_replacement(old: &ClaimRecord, holder: &str) -> ClaimRecord {
        let mut fresh = old.clone();
        fresh.holder = holder.to_string();
        fresh.acquired_at = claims::now_ms();
        fresh.pid = Some(std::process::id() as i32);
        fresh.session_id = None;
        fresh
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

    #[test]
    fn reap_one_refuses_a_fresh_replacement_after_a_stale_scan() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "node:reap-race-test";
            let mut old = match claims::acquire(
                key,
                "target-session:old",
                claims::AcquireOpts {
                    pid: Some(reaped_pid()),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            old.session_id = None;
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();
            let lock = claims::recovery_lock_path(&path);
            let token = claims::acquire_dir_mutex(&lock, Duration::from_secs(2), true).unwrap();
            let fresh = live_replacement(&old, "target-session:fresh");
            std::fs::write(&path, claims::serialize_claim(&fresh).unwrap()).unwrap();
            claims::release_dir_mutex(&lock, &token);

            assert!(!reap_one(&path, &old).unwrap());
            assert_eq!(
                claims::status(key, Some(temp.path())).1.unwrap().holder,
                fresh.holder
            );
        });
    }

    #[test]
    fn release_waits_for_recovery_and_preserves_a_replacement_holder() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "node:release-race-test";
            let mut old = match claims::acquire(
                key,
                "target-session:old",
                claims::AcquireOpts {
                    pid: Some(reaped_pid()),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            old.session_id = None;
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();
            let lock = claims::recovery_lock_path(&path);
            let token = claims::acquire_dir_mutex(&lock, Duration::from_secs(2), true).unwrap();
            let root = temp.path().to_path_buf();
            let release_key = key.to_string();
            let holder = old.holder.clone();
            let release = thread::spawn(move || {
                claims::release(&release_key, &holder, Some(&root), Some(&root))
            });
            thread::sleep(Duration::from_millis(200));
            let waited_for_lock = !release.is_finished();

            let fresh = live_replacement(&old, "target-session:fresh");
            std::fs::write(&path, claims::serialize_claim(&fresh).unwrap()).unwrap();
            claims::release_dir_mutex(&lock, &token);

            release.join().unwrap().unwrap();
            assert!(
                waited_for_lock,
                "release ignored the per-key recovery mutex"
            );
            assert_eq!(
                claims::status(key, Some(temp.path())).1.unwrap().holder,
                fresh.holder
            );
        });
    }

    #[test]
    fn release_receipt_returns_only_the_claim_it_unlinked() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "node:release-receipt-test";
            let root = Some(temp.path().to_path_buf());
            let acquired = match claims::acquire(
                key,
                "target-session:owner",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };

            let released =
                claims::release_with_receipt(key, &acquired.holder, Some(temp.path()), None)
                    .unwrap()
                    .unwrap();
            assert_eq!(released.acquired_at, acquired.acquired_at);
            assert!(
                claims::release_with_receipt(key, &acquired.holder, Some(temp.path()), None,)
                    .unwrap()
                    .is_none()
            );

            let replacement = match claims::acquire(
                key,
                "target-session:replacement",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root,
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("replacement fixture failed: {other:?}"),
            };
            assert!(claims::release_with_receipt(
                key,
                "target-session:foreign",
                Some(temp.path()),
                None,
            )
            .unwrap()
            .is_none());
            assert_eq!(
                claims::status(key, Some(temp.path())).1.unwrap().holder,
                replacement.holder
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn strict_claim_listing_refuses_symlinked_lockfiles() {
        let temp = TempDir::new().unwrap();
        let path = claims::claim_path("node:x-symlink", Some(temp.path())).unwrap();
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory).unwrap();
        let target = temp.path().join("claim-target");
        std::fs::write(&target, "not a lockfile").unwrap();
        std::os::unix::fs::symlink(target, &path).unwrap();

        let error =
            claims::list_in_strict(&[directory.to_path_buf()], Some("node:"), true).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn strict_claim_listing_refuses_a_lockfile_with_a_mismatched_key() {
        let temp = TempDir::new().unwrap();
        let root = Some(temp.path().to_path_buf());
        let record = match claims::acquire(
            "node:x-original",
            "target-session:owner",
            claims::AcquireOpts {
                pid: Some(std::process::id()),
                root: root.clone(),
                ..Default::default()
            },
        ) {
            claims::AcquireOutcome::Acquired(record) => record,
            other => panic!("claim fixture failed: {other:?}"),
        };
        let wrong_path = claims::claim_path("node:x-mismatch", Some(temp.path())).unwrap();
        std::fs::write(&wrong_path, claims::serialize_claim(&record).unwrap()).unwrap();
        let directory = wrong_path.parent().unwrap();

        let error =
            claims::list_in_strict(&[directory.to_path_buf()], Some("node:"), true).unwrap_err();
        assert!(error.contains("filename does not match key"), "{error}");
    }

    #[test]
    fn strict_node_listing_refuses_a_node_filename_with_a_non_node_claim() {
        let temp = TempDir::new().unwrap();
        let record = match claims::acquire(
            "task:x-original:1.1",
            "target-session:owner",
            claims::AcquireOpts {
                pid: Some(std::process::id()),
                root: Some(temp.path().to_path_buf()),
                ..Default::default()
            },
        ) {
            claims::AcquireOutcome::Acquired(record) => record,
            other => panic!("claim fixture failed: {other:?}"),
        };
        let node_path = claims::claim_path("node:x-mismatch", Some(temp.path())).unwrap();
        std::fs::write(&node_path, claims::serialize_claim(&record).unwrap()).unwrap();

        let error = claims::list_in_strict(
            &[node_path.parent().unwrap().to_path_buf()],
            Some("node:"),
            true,
        )
        .unwrap_err();
        assert!(error.contains("filename does not match key"), "{error}");
    }

    #[test]
    fn task_acquire_keeps_an_expired_claim_when_its_session_is_live() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "task:x-session-witness:1.1";
            let root = Some(temp.path().to_path_buf());
            let mut old = match claims::acquire(
                key,
                "target-session:thread-holder",
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(60_000),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let now = claims::now_ms();
            old.acquired_at = now - 120_000;
            old.expires_at = Some(now - 60_000);
            old.pid_provenance = Some("ambient".into());
            old.session_id = Some("thread-session".into());
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();

            let live = |_| claims::SessionLiveness::Live("test-session-live");
            let live_witness: claims::SessionWitness<'_> = &live;
            let outcome = claims::acquire_with_session_witness(
                key,
                "target-session:second",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root: root.clone(),
                    ..Default::default()
                },
                Some(live_witness),
            );
            assert!(
                matches!(outcome, claims::AcquireOutcome::HeldByOther { holder, .. } if holder == old.holder),
                "live thread claim was stolen: {outcome:?}"
            );

            old.expires_at = Some(now + 60_000);
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();
            let absent = |_| claims::SessionLiveness::Absent;
            let absent_witness: claims::SessionWitness<'_> = &absent;
            let outcome = claims::acquire_with_session_witness(
                key,
                "target-session:second",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root: root.clone(),
                    ..Default::default()
                },
                Some(absent_witness),
            );
            assert!(
                matches!(outcome, claims::AcquireOutcome::Acquired(_)),
                "absent thread claim stayed held: {outcome:?}"
            );

            let race_key = "task:x-session-witness:1.3";
            let mut raced_claim = match claims::acquire(
                race_key,
                "target-session:thread-holder",
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(60_000),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            raced_claim.acquired_at = now - 120_000;
            raced_claim.expires_at = Some(now - 60_000);
            raced_claim.pid_provenance = Some("ambient".into());
            raced_claim.session_id = Some("thread-session-race".into());
            let race_path = claims::claim_path(race_key, Some(temp.path())).unwrap();
            std::fs::write(&race_path, claims::serialize_claim(&raced_claim).unwrap()).unwrap();

            let observations = std::cell::Cell::new(0usize);
            let becomes_live = |_| {
                let count = observations.get();
                observations.set(count + 1);
                if count == 0 {
                    claims::SessionLiveness::Absent
                } else {
                    claims::SessionLiveness::Live("test-session-live")
                }
            };
            let witness: claims::SessionWitness<'_> = &becomes_live;
            let outcome = claims::acquire_with_session_witness(
                race_key,
                "target-session:second",
                claims::AcquireOpts {
                    pid: Some(std::process::id()),
                    root,
                    ..Default::default()
                },
                Some(witness),
            );
            assert!(
                matches!(outcome, claims::AcquireOutcome::HeldByOther { holder, .. } if holder == raced_claim.holder),
                "newly live thread claim was stolen: {outcome:?}"
            );
            assert!(
                observations.get() >= 2,
                "acquire did not recheck under lock"
            );
        });
    }

    #[test]
    fn task_reap_rechecks_session_liveness_under_recovery_lock() {
        let temp = TempDir::new().unwrap();
        with_claims_root(temp.path(), || {
            let key = "task:x-session-witness:1.2";
            let root = Some(temp.path().to_path_buf());
            let mut old = match claims::acquire(
                key,
                "target-session:thread-holder",
                claims::AcquireOpts {
                    pid_unavailable: true,
                    ttl_ms: Some(60_000),
                    root: root.clone(),
                    ..Default::default()
                },
            ) {
                claims::AcquireOutcome::Acquired(record) => record,
                other => panic!("claim fixture failed: {other:?}"),
            };
            let now = claims::now_ms();
            old.acquired_at = now - 120_000;
            old.expires_at = Some(now - 60_000);
            old.pid_provenance = Some("ambient".into());
            old.session_id = Some("thread-session-reap".into());
            let path = claims::claim_path(key, Some(temp.path())).unwrap();
            std::fs::write(&path, claims::serialize_claim(&old).unwrap()).unwrap();

            let observations = std::cell::Cell::new(0usize);
            let becomes_live = |_| {
                let count = observations.get();
                observations.set(count + 1);
                if count == 0 {
                    claims::SessionLiveness::Absent
                } else {
                    claims::SessionLiveness::Live("test-session-live")
                }
            };
            let witness: claims::SessionWitness<'_> = &becomes_live;
            let result =
                reap_with_session_witness(root.as_deref(), true, Some(witness), Some(witness))
                    .unwrap();
            assert_eq!(
                result["reaped"], 0,
                "live thread claim was reaped: {result}"
            );
            assert!(path.exists(), "live thread claim file was archived");
            assert!(observations.get() >= 2, "reaper did not recheck under lock");
        });
    }
}
