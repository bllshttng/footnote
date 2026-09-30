use crate::paths::AgentsHome;

/// Resolve one registry entry by harness session id and stop it through the
/// same confirmed worker path used by `fno agents stop`.
pub(crate) async fn stop_session_for_home(
    home: &AgentsHome,
    session_id: &str,
) -> Option<(String, bool)> {
    let registry = super::load_registry_offloaded(home.registry_json())
        .await
        .ok()?;
    let mut matches = registry.entries.iter().filter(|entry| {
        entry.harness_session_id.as_deref() == Some(session_id)
            || entry.session_id.as_deref() == Some(session_id)
    });
    let entry = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let name = entry.name.clone();
    let stopped = super::stop_worker_confirmed_for_home(home, entry).await;
    Some((name, stopped))
}
