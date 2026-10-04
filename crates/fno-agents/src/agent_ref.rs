//! The one agent-identity join (ruling d-f9c59b68): a row is keyed by its
//! fno_id and harness session id, and the fleet name is display only.
//! `Key::Id` answers from identity alone; `Key::Name` is an edge input (the
//! label, a prior label, or the 8-hex short id, which is a prefix and can
//! collide) that refuses a second match instead of guessing.

use crate::state::RegistryEntry;

/// The identity card of one registry row: the keys every join resolves
/// through, the name that renders, and the links the identity ruling names
/// (no writer emits links yet; the field travels so readers can).
pub struct AgentRef {
    pub fno_id: Option<String>,
    pub harness_session_id: Option<String>,
    pub name: String,
    pub links: Vec<String>,
}

impl From<&RegistryEntry> for AgentRef {
    fn from(row: &RegistryEntry) -> Self {
        Self {
            fno_id: row.fno_id.clone(),
            harness_session_id: row.harness_session_id.clone(),
            name: row.name.clone(),
            links: Vec::new(),
        }
    }
}

/// What a caller holds. `Id` is a stored identity (a full fno_id, harness
/// session id, or related session id); `Name` is what an edge was handed.
pub enum Key<'a> {
    Id(&'a str),
    Name(&'a str),
}

pub enum Join<'a> {
    One(&'a RegistryEntry),
    Ambiguous,
    None,
}

/// The id-first join. A second match under either key refuses: the first
/// match of a duplicate is a guess, and the callers act on the row.
pub fn resolve<'a, F>(rows: &'a [RegistryEntry], key: Key<'_>, keep: F) -> Join<'a>
where
    F: Fn(&RegistryEntry) -> bool,
{
    let mut matches = rows.iter().filter(|row| {
        keep(row)
            && match key {
                Key::Id(id) => {
                    row.fno_id.as_deref() == Some(id)
                        || row.harness_session_id.as_deref() == Some(id)
                        || row.related_session_id.as_deref() == Some(id)
                }
                Key::Name(name) => {
                    row.name == name
                        || row.aliases.iter().any(|alias| alias == name)
                        || (!row.short_id.is_empty() && row.short_id == name)
                }
            }
    });
    match (matches.next(), matches.next()) {
        (Some(row), None) => Join::One(row),
        (None, _) => Join::None,
        (Some(_), Some(_)) => Join::Ambiguous,
    }
}

/// An untagged address (a CLI token that may be an id or a name): identity
/// answers first, the name tier only when no row answers to the id, and a
/// second match anywhere refuses - the same refusal the mail address join
/// has always given.
pub fn resolve_address<'a, F>(rows: &'a [RegistryEntry], address: &str, keep: F) -> Join<'a>
where
    F: Fn(&RegistryEntry) -> bool + Copy,
{
    let by_id = resolve(rows, Key::Id(address), keep);
    let by_name = resolve(rows, Key::Name(address), keep);
    match (by_id, by_name) {
        (Join::One(row), Join::None) | (Join::None, Join::One(row)) => Join::One(row),
        (Join::One(row), Join::One(same)) if std::ptr::eq(row, same) => Join::One(row),
        (Join::None, Join::None) => Join::None,
        _ => Join::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentStatus;

    fn entry(name: &str, session: &str, extra: serde_json::Value) -> RegistryEntry {
        let mut row = serde_json::json!({
            "name": name,
            "cwd": "/tmp",
            "status": "live",
            "created_at": "2026-10-04T00:00:00Z",
            "harness": "claude",
            "harness_session_id": session,
        });
        for (key, value) in extra.as_object().unwrap() {
            row[key] = value.clone();
        }
        serde_json::from_value(row).unwrap()
    }

    #[test]
    fn an_id_matches_identity_never_a_name() {
        let rows = vec![
            entry("king", "sess-1", serde_json::json!({"fno_id": "f-1"})),
            // A row NAMED like another row's id must not answer an Id key.
            entry("f-1", "sess-2", serde_json::json!({})),
        ];
        let got = resolve(&rows, Key::Id("f-1"), |_| true);
        assert!(matches!(got, Join::One(row) if std::ptr::eq(row, &rows[0])));
        assert!(matches!(
            resolve(&rows, Key::Id("sess-2"), |_| true),
            Join::One(_)
        ));
        assert!(matches!(resolve(&rows, Key::Id("nobody"), |_| true), Join::None));
    }

    #[test]
    fn a_shared_name_refuses() {
        let rows = vec![
            entry("dupe", "sess-1", serde_json::json!({})),
            entry("dupe", "sess-2", serde_json::json!({})),
        ];
        assert!(matches!(
            resolve(&rows, Key::Name("dupe"), |_| true),
            Join::Ambiguous
        ));
    }

    #[test]
    fn an_alias_and_a_short_id_answer_at_the_name_tier() {
        let rows = vec![entry(
            "new",
            "sess-1",
            serde_json::json!({"aliases": ["old"], "short_id": "abcd1234"}),
        )];
        assert!(matches!(resolve(&rows, Key::Name("old"), |_| true), Join::One(_)));
        assert!(matches!(
            resolve(&rows, Key::Name("abcd1234"), |_| true),
            Join::One(_)
        ));
    }

    #[test]
    fn the_liveness_filter_decides_who_answers() {
        let rows = vec![
            entry("dupe", "sess-1", serde_json::json!({})),
            entry("dupe", "sess-2", serde_json::json!({"status": "exited"})),
        ];
        let got = resolve(&rows, Key::Name("dupe"), |row| row.status != AgentStatus::Exited);
        assert!(matches!(got, Join::One(row) if row.harness_session_id.as_deref() == Some("sess-1")));
    }

    #[test]
    fn an_address_resolves_and_a_cross_tier_second_match_refuses() {
        let one = vec![entry("king", "sess-1", serde_json::json!({}))];
        assert!(matches!(
            resolve_address(&one, "sess-1", |_| true),
            Join::One(_)
        ));
        assert!(matches!(resolve_address(&one, "king", |_| true), Join::One(_)));
        let twin = vec![
            entry("x", "sess-1", serde_json::json!({})),
            entry("y", "x", serde_json::json!({})),
        ];
        assert!(matches!(
            resolve_address(&twin, "x", |_| true),
            Join::Ambiguous
        ));
    }
}
