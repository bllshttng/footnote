//! The kind of a spawn edge. A CHILD edge means the spawner orchestrates
//! and waits: a king over its court, a lead over its join workers. A PEER
//! edge is a handoff: a blueprint launching its target, an advance dispatch
//! starting the next node. A PEER spawner is done and waits on nothing.
//!
//! The kind is derived, never stored at mint time: a CHILD is a row with a
//! joiner name, or a row whose spawner was crowned. Everything else reads
//! PEER. This module is the rule's single owner; the reaper calls
//! [`lineage_kind`] through [`live_child_of`] directly, and the liveness
//! sweep stamps the derived word onto rows through [`stamp_lineage_kinds`]
//! for readers that cannot call this crate.

use crate::state::{Lineage, RegistryEntry};

/// Whether a spawn edge means "waits on it" (Child) or "handed off to it"
/// (Peer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageKind {
    Child,
    Peer,
}

impl LineageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LineageKind::Child => "child",
            LineageKind::Peer => "peer",
        }
    }
}

/// The kind of the edge from `parent_crowned` to the row named
/// `child_name`: crowned spawners hold their court, join workers
/// (`jn-t-`, legacy `j-`) hold their lead, and every other edge is a
/// handoff that holds nobody.
pub fn lineage_kind(child_name: &str, parent_crowned: bool) -> LineageKind {
    if parent_crowned || child_name.starts_with("jn-t-") || child_name.starts_with("j-") {
        LineageKind::Child
    } else {
        LineageKind::Peer
    }
}

/// The one live CHILD row `parent` waits on, if any. A row qualifies when
/// its name differs, its `spawned_by_session` names the parent's harness
/// session, the edge is CHILD, and the row can still hold a process: a
/// liveish status whose served liveness word is not `dead`. A PEER row
/// never qualifies, so a handoff holds its spawner for nothing.
pub fn live_child_of<'a>(
    parent: &RegistryEntry,
    entries: &'a [RegistryEntry],
) -> Option<&'a RegistryEntry> {
    let sid = parent.harness_session_id.as_deref().unwrap_or("").trim();
    if sid.is_empty() {
        return None;
    }
    let sid_lower = sid.to_ascii_lowercase();
    entries.iter().find(|child| {
        child.name != parent.name
            && child
                .spawned_by_session
                .as_deref()
                .is_some_and(|s| s.trim().to_ascii_lowercase() == sid_lower)
            && lineage_kind(&child.name, parent.crown_level.is_some()) == LineageKind::Child
            && crate::spawn_gate::status_is_liveish(&child.status)
            && crate::row_truth::served_fresh_liveness(
                child.liveness.as_deref(),
                child.liveness_measured_at.as_deref(),
            ) != Some("dead")
    })
}

/// The one `agent_spawned` payload builder. A birth names its parent or
/// says why it could not - the rule `registry_schema.toml` states in both
/// trees and `registry.py::mint_agent_entry` enforces on the Python leg.
/// `extras` carries the per-door keys (`provider`, `lane`, `substrate`, ...)
/// and merges over the lineage keys, which never collide. A lineage with
/// neither a session nor a reason reads as `"birth built with no lineage"`:
/// the journal never says nothing. The birth guard (`birth_guard_tests.rs`)
/// fails any emit of that kind that bypasses this builder.
pub fn birth_event(name: &str, lineage: &Lineage, extras: serde_json::Value) -> serde_json::Value {
    let session = lineage
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let reason = lineage
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut payload = serde_json::json!({
        "name": name,
        "spawned_by_session": session,
        "spawned_by_harness": lineage
            .harness
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        "spawned_by_cwd": lineage
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        "lineage_reason": reason,
    });
    if session.is_none() && reason.is_none() {
        payload["lineage_reason"] = serde_json::Value::String("birth built with no lineage".into());
    }
    if let (Some(base), Some(rest)) = (payload.as_object_mut(), extras.as_object()) {
        for (k, v) in rest {
            base.insert(k.clone(), v.clone());
        }
    }
    payload
}

/// Stamp the derived kind onto every row with a spawn edge, for readers
/// outside this crate (the sideline links crate `fno`, which cannot depend
/// on fno-agents). The liveness sweep calls this inside its lock window;
/// nothing else writes the field.
pub(crate) fn stamp_lineage_kinds(r: &mut crate::state::Registry) {
    let crowned: std::collections::HashSet<String> = r
        .entries
        .iter()
        .filter(|e| e.crown_level.is_some())
        .filter_map(|e| {
            e.harness_session_id
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
        })
        .filter(|s| !s.is_empty())
        .collect();
    for row in r.entries.iter_mut() {
        let Some(edge) = row
            .spawned_by_session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let kind = lineage_kind(&row.name, crowned.contains(&edge.to_ascii_lowercase()));
        if row.lineage_kind.as_deref() != Some(kind.as_str()) {
            row.lineage_kind = Some(kind.as_str().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, sid: &str, spawned_by: Option<&str>) -> RegistryEntry {
        let mut e = RegistryEntry::default();
        e.name = name.into();
        e.harness_session_id = Some(sid.into());
        e.spawned_by_session = spawned_by.map(String::from);
        e
    }

    #[test]
    fn kind_table() {
        assert_eq!(lineage_kind("jn-t-x-1-1", false), LineageKind::Child);
        assert_eq!(lineage_kind("j-x-1-1", false), LineageKind::Child);
        assert_eq!(lineage_kind("node-x-demo2-g2", true), LineageKind::Child);
        assert_eq!(lineage_kind("sob-t-x-2-glm", false), LineageKind::Peer);
        assert_eq!(lineage_kind("ac-t-x-3-opus", false), LineageKind::Peer);
        assert_eq!(lineage_kind("bp-x-4-repro", false), LineageKind::Peer);
    }

    #[test]
    fn live_child_of_finds_a_live_child_by_session() {
        let parent = row("t-x-demo-lead", "s-lead", None);
        let mut joiner = row("jn-t-x-demo-1", "s-j1", Some("S-LEAD "));
        joiner.status = crate::AgentStatus::Busy;
        let entries = vec![parent.clone(), joiner];
        let found = live_child_of(&parent, &entries).expect("the joiner holds the lead");
        assert_eq!(found.name, "jn-t-x-demo-1");
    }

    #[test]
    fn a_peer_row_never_holds_its_spawner() {
        let parent = row("bp-x-1", "s-bp", None);
        let mut handoff = row("sob-t-x-1-glm", "s-t1", Some("s-bp"));
        handoff.status = crate::AgentStatus::Busy;
        let entries = vec![parent.clone(), handoff];
        assert!(live_child_of(&parent, &entries).is_none());
    }

    #[test]
    fn an_exited_or_dead_child_releases_its_parent() {
        let parent = row("t-x-2-lead", "s-lead", None);
        let mut exited = row("jn-t-x-2-1", "s-j1", Some("s-lead"));
        exited.status = crate::AgentStatus::Exited;
        let mut dead = row("jn-t-x-2-2", "s-j2", Some("s-lead"));
        dead.status = crate::AgentStatus::Busy;
        dead.liveness = Some("dead".into());
        dead.liveness_measured_at = Some(chrono_now_rfc3339());
        let entries = vec![parent.clone(), exited, dead];
        assert!(live_child_of(&parent, &entries).is_none());
    }

    #[test]
    fn a_stale_liveness_word_is_not_dead_evidence() {
        let parent = row("t-x-3-lead", "s-lead", None);
        let mut joiner = row("jn-t-x-3-1", "s-j1", Some("s-lead"));
        joiner.status = crate::AgentStatus::Busy;
        // A word with no stamp (never measured) withholds the served word
        // entirely; the status decides.
        joiner.liveness = Some("busy".into());
        let entries = vec![parent.clone(), joiner];
        assert!(live_child_of(&parent, &entries).is_some());
    }

    #[test]
    fn an_empty_parent_session_never_matches() {
        let parent = row("t-x-4-lead", "  ", None);
        let mut joiner = row("jn-t-x-4-1", "s-j1", Some("s-lead"));
        joiner.status = crate::AgentStatus::Busy;
        let entries = vec![parent.clone(), joiner];
        assert!(live_child_of(&parent, &entries).is_none());
    }

    #[test]
    fn a_crowned_parent_reads_its_court_as_child() {
        let mut king = row("king-x-5", "s-king", None);
        king.crown_level = Some(1);
        let mut court = row("node-x-demo2-g2", "s-court", Some("s-king"));
        court.status = crate::AgentStatus::Busy;
        let entries = vec![king.clone(), court.clone()];
        let found = live_child_of(&king, &entries).expect("the court row is a child");
        assert_eq!(found.name, "node-x-demo2-g2");
    }

    #[test]
    fn stamp_writes_child_peer_and_leaves_edgeless_rows_silent() {
        let mut r = crate::state::Registry::default();
        let mut king = row("king-x-6", "s-king", None);
        king.crown_level = Some(1);
        let mut court = row("node-x-demo2-g2", "s-court", Some("s-king"));
        court.status = crate::AgentStatus::Busy;
        let mut joiner = row("jn-t-x-1-1", "s-j", Some("s-lead"));
        joiner.status = crate::AgentStatus::Busy;
        let mut handoff = row("sob-t-x-2-glm", "s-t", Some(" s-lead "));
        handoff.status = crate::AgentStatus::Busy;
        let orphan = row("sob-t-x-3-glm", "s-t3", Some("s-gone"));
        let plain = row("solo-x-7", "s-solo", None);
        r.entries = vec![king, court, joiner, handoff, orphan, plain];
        stamp_lineage_kinds(&mut r);
        let kinds: Vec<Option<&str>> = r
            .entries
            .iter()
            .map(|e| e.lineage_kind.as_deref())
            .collect();
        assert_eq!(
            kinds,
            vec![
                None,
                Some("child"),
                Some("child"),
                Some("peer"),
                Some("peer"),
                None
            ]
        );
    }

    #[test]
    fn stamp_is_idempotent_and_never_lowers_a_written_word() {
        let mut r = crate::state::Registry::default();
        let mut handoff = row("sob-t-x-4-glm", "s-t4", Some("s-lead"));
        handoff.lineage_kind = Some("peer".into());
        r.entries = vec![handoff];
        stamp_lineage_kinds(&mut r);
        assert_eq!(r.entries[0].lineage_kind.as_deref(), Some("peer"));
    }

    fn chrono_now_rfc3339() -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    #[test]
    fn birth_event_names_the_parent_with_the_triple() {
        let lineage = Lineage::captured((
            Some(" s-parent ".into()),
            Some("claude".into()),
            Some("/repo".into()),
        ));
        let payload = birth_event(
            "t-x-worker",
            &lineage,
            serde_json::json!({"provider": "codex", "lane": "thread"}),
        );
        assert_eq!(payload["name"], "t-x-worker");
        assert_eq!(payload["spawned_by_session"], "s-parent");
        assert_eq!(payload["spawned_by_harness"], "claude");
        assert_eq!(payload["spawned_by_cwd"], "/repo");
        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["lane"], "thread");
    }

    #[test]
    fn a_birth_without_a_session_carries_its_reason() {
        let lineage = Lineage::unproven("daemon mint: spawn request carried no parent edge");
        let payload = birth_event("t-x-worker", &lineage, serde_json::json!({}));
        assert!(payload["spawned_by_session"].is_null());
        assert_eq!(
            payload["lineage_reason"],
            "daemon mint: spawn request carried no parent edge"
        );
    }

    #[test]
    fn a_birth_with_neither_session_nor_reason_never_says_nothing() {
        let payload = birth_event(
            "t-x-worker",
            &Lineage::captured((None, None, None)),
            serde_json::json!({"provider": "opencode"}),
        );
        assert!(payload["spawned_by_session"].is_null());
        assert_eq!(payload["lineage_reason"], "birth built with no lineage");
        assert_eq!(payload["provider"], "opencode");
    }
}
