//! The derived parent NAME for a merged row set.

use super::RegistryAgent;

/// Derive [`RegistryAgent::spawned_by_name`] once for the whole row set:
/// each row's `spawned_by_session` is joined, case- and whitespace-
/// insensitive (edges arrive with stray case; the same tolerance
/// `spawn_edge::live_child_of` applies), to the row whose
/// `harness_session_id` it names. An id two DIFFERENT names claim maps to
/// no name: an ambiguous parent reads as absent, never as a confident
/// wrong answer. An edge naming a session no row holds also reads as
/// absent - that absence is a fact the reader must be able to see.
pub(super) fn derive_spawned_by_name(rows: &mut [RegistryAgent]) {
    use std::collections::{HashMap, HashSet};
    let mut name_by_sid: HashMap<String, String> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    for r in rows.iter() {
        let Some(sid) = r
            .harness_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let key = sid.to_ascii_lowercase();
        match name_by_sid.get(&key) {
            Some(prev) if prev != &r.name => {
                name_by_sid.remove(&key);
                ambiguous.insert(key);
            }
            Some(_) => {}
            None => {
                name_by_sid.insert(key, r.name.clone());
            }
        }
    }
    for r in rows.iter_mut() {
        let Some(edge) = r
            .spawned_by_session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let key = edge.to_ascii_lowercase();
        r.spawned_by_name = if ambiguous.contains(&key) {
            None
        } else {
            name_by_sid.get(&key).cloned()
        };
    }
}
