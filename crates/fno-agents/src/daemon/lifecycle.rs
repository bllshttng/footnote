//! Identity resolution behind the lifecycle verbs (stop/rm). Split from
//! daemon.rs for the file budget.

use serde_json::{json, Value};

/// Resolve the lifecycle target the way the session-connecting verbs do:
/// registry first, then a best-effort harness-store heal for session-shaped
/// tokens. `cross_project` lifts the store heal's project confinement, which
/// is the grant `fno agents rm --cross-project` forwards. `for_stop` tries the
/// rm tombstone FIRST: a session `fno agents rm` just removed resolves from
/// the tombstone the daemon itself stamped (short id, full session id, cwd),
/// never through the adopting heal, whose rm grace window refuses exactly
/// this session and whose adoption would resurrect the removed row. rm and
/// every other caller keep the adopting heal. The caller's pre-heal registry
/// snapshot rides in because the helper may have just adopted a store-only
/// session absent from that snapshot.
pub(crate) async fn entry_for_lifecycle(
    registry: &crate::state::Registry,
    token: &str,
    registry_path: &std::path::Path,
    cross_project: bool,
    for_stop: bool,
) -> Result<Option<crate::state::RegistryEntry>, String> {
    let Value::Array(rows) = serde_json::to_value(&registry.entries)
        .map_err(|exc| format!("could not inspect registry identities: {exc}"))?
    else {
        return Err("could not inspect registry identities".to_string());
    };
    let worker_token = token.to_string();
    let path = registry_path.to_path_buf();
    let resolved = tokio::task::spawn_blocking(move || {
        if for_stop {
            if let Some(removed) = crate::rm_tombstone::lookup_removed_beside(&path, &worker_token)
            {
                return Ok(Some(json!({
                    "name": if removed.name.is_empty() {
                        crate::identity::canonical_handle(&removed.session_id)
                    } else {
                        removed.name
                    },
                    "short_id": removed.short,
                    "harness": removed.harness,
                    "harness_session_id": removed.session_id,
                    "cwd": removed.cwd,
                    "host_mode": if removed.host_mode.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(removed.host_mode)
                    },
                    "status": "orphaned",
                    "origin": "adopted",
                    "created_at": crate::daemon::now_rfc3339_like(),
                })));
            }
        }
        crate::client_verbs::resolve_entry_with_heal_scoped(
            &rows,
            &worker_token,
            &path,
            cross_project,
            None,
        )
        .map(Some)
        .map_err(|err| err.message())
    })
    .await
    .map_err(|exc| format!("identity resolution task failed: {exc}"))?;
    match resolved {
        Ok(Some(entry)) => {
            let mut entry: crate::state::RegistryEntry = serde_json::from_value(entry)
                .map_err(|exc| format!("resolved identity row is unreadable: {exc}"))?;
            entry.backfill_harness_aliases();
            if let Some(legacy) = entry.backfill_short_id() {
                return Err(format!(
                    "resolved identity row {:?} has conflicting transport ids (legacy={legacy:?})",
                    entry.name
                ));
            }
            Ok(Some(entry))
        }
        Ok(None) => Ok(None),
        Err(message) => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session `fno agents rm` just removed resolves for STOP from the
    /// tombstone the rm stamped (short id, full session id, cwd), not from the
    /// adopting store heal whose rm grace window refuses exactly this session
    /// and whose adoption would resurrect the removed row.
    #[tokio::test]
    async fn lifecycle_stop_resolves_a_removed_session_from_the_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("rm_tombstones.json"),
            serde_json::json!([{
                "harness": "codex",
                "session_id": "0198cccc-0000-0000-0000-000000000003",
                "short": "f00dcafe",
                "name": "t-removed",
                "cwd": "/repo/two",
                "host_mode": "interactive",
                "removed_at": super::super::now_epoch_secs(),
            }])
            .to_string(),
        )
        .unwrap();
        let reg = crate::state::Registry {
            schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
            entries: vec![],
            ..Default::default()
        };

        let entry =
            entry_for_lifecycle(&reg, "f00dcafe", &agents.join("registry.json"), false, true)
                .await
                .expect("the tombstone resolves the removed session")
                .expect("the token names a recorded removal");

        assert_eq!(entry.name, "t-removed");
        assert_eq!(entry.harness_name(), "codex");
        assert_eq!(
            entry.harness_session_id.as_deref(),
            Some("0198cccc-0000-0000-0000-000000000003")
        );
        assert_eq!(entry.cwd, "/repo/two");
        // The recorded host mode rides along so the synthesized row passes the
        // strict codex-thread gate; a codex ask row must not.
        assert_eq!(entry.host_mode_or_default(), "interactive");

        std::fs::remove_dir_all(dir.keep()).ok();
    }
}
