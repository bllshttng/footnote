//! Identity resolution behind the lifecycle verbs (stop/rm). Split from
//! daemon.rs for the file budget.

use serde_json::Value;

/// Resolve the lifecycle target the way the session-connecting verbs do:
/// registry first, then a best-effort harness-store heal for session-shaped
/// tokens. `cross_project` lifts the store heal's project confinement, which
/// is the grant `fno agents rm --cross-project` forwards. `for_stop` rides
/// the heal without adopting: the rm tombstone grace window does not apply
/// and nothing is registered, so a stop can reach a session `fno agents rm`
/// just removed (the rm keeps the adopting heal). The caller's pre-heal
/// registry snapshot rides in because the helper may have just adopted a
/// store-only session absent from that snapshot.
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
        crate::client_verbs::resolve_entry_with_heal_scoped(
            &rows,
            &worker_token,
            &path,
            cross_project,
            None,
            for_stop,
        )
    })
    .await
    .map_err(|exc| format!("identity resolution task failed: {exc}"))?;
    match resolved {
        Ok(entry) => {
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
        Err(crate::client_verbs::ResolveError::NotFound(_)) => Ok(None),
        Err(err) => Err(err.message()),
    }
}
