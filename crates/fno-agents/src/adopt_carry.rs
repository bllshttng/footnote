//! The adopt upsert's team-carry rule: a synthesized adopt observed nothing
//! about authority, so the merge keeps whatever a spawn, register or grantor
//! path stamped. Dropping it is the registry-restore shape that unpromoted a
//! live fleet - rows stayed, their teams did not, and no team could be
//! granted until an attended shell re-stamped.

use crate::state::RegistryEntry;

/// Fill the incoming row's empty team fields from the row it replaces.
/// One direction only: a synthesized row that DOES carry a team (a manifest
/// adopt) outranks the stale copy it replaces.
pub(crate) fn carry_adopted_team(merged: &mut RegistryEntry, old: &RegistryEntry) {
    if merged.role_level.is_none() {
        merged.role_level = old.role_level;
    }
    if merged.role_scope.is_none() {
        merged.role_scope = old.role_scope.clone();
    }
    if merged.role_grantor.is_none() {
        merged.role_grantor = old.role_grantor.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn teamed_entry(session: &str) -> RegistryEntry {
        let mut e = RegistryEntry::default();
        e.name = "lead".into();
        e.harness = Some("codex".into());
        e.harness_session_id = Some(session.into());
        e.role_level = Some(2);
        e.role_scope = Some("x-a,x-b".into());
        e.role_grantor = Some("vellum".into());
        e
    }

    /// An upsert over a teamed row keeps the team: the merge preserves the
    /// same class of fact it already preserves for pid, status and node.
    #[test]
    fn upsert_synthesized_row_carries_team_forward() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = dir.path().join("registry.json");
        let teamed = teamed_entry("thread-team");
        crate::client_verbs::upsert_synthesized_row(&reg, teamed).unwrap();

        let mut replacement = RegistryEntry::default();
        replacement.name = "lead".into();
        replacement.harness = Some("codex".into());
        replacement.harness_session_id = Some("thread-team".into());
        replacement.created_at = "t2".into();
        crate::client_verbs::upsert_synthesized_row(&reg, replacement).unwrap();

        let loaded = crate::state::load_registry(&reg).unwrap();
        let row = loaded
            .entries
            .iter()
            .find(|r| r.harness_session_id.as_deref() == Some("thread-team"))
            .expect("row survives");
        assert_eq!(row.role_level, Some(2));
        assert_eq!(row.role_scope.as_deref(), Some("x-a,x-b"));
        assert_eq!(row.role_grantor.as_deref(), Some("vellum"));
    }

    /// A synthesized row that carries its own team outranks the stale copy.
    #[test]
    fn an_incoming_team_is_not_overwritten() {
        let mut merged = RegistryEntry::default();
        merged.role_scope = Some("fresh".into());
        let old = teamed_entry("s-old");
        carry_adopted_team(&mut merged, &old);
        assert_eq!(merged.role_scope.as_deref(), Some("fresh"));
        assert_eq!(merged.role_level, Some(2));
    }
}
