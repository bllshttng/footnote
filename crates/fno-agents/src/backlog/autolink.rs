//! Rollup resolution: which epic does a plan-less filing serve, ported from
//! `cli/src/fno/graph/rollup.py` (`resolve` + `receipt_lines` + the team
//! ladder). Metadata-only: writes `parent` and nothing else, and an orphan
//! is never refused. The caller applies a linked result inside the locked
//! write; this module only scores and decides.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::node_ref::{find_node, would_create_cycle, would_exceed_epic_depth};
use super::relatedness::epic_candidates;

/// Only these types roll up. Bugs, epics, and roadmap containers are exempt
/// by their kind - a bug serves a defect, not a mission.
pub const ROLLUP_TYPES: [&str; 2] = ["feature", "task"];

/// Work that is over. A shipped feature with no mission edge is history, not
/// a rollup an operator can still make.
pub const CLOSED_STATUSES: [&str; 3] = ["done", "superseded", "deferred"];

/// Auto-link bar. Deliberately high, and margin-gated so two plausible epics
/// never coin-flip a parent edge - below either bar we suggest instead.
const AUTO_LINK_MIN: f64 = 0.55;
const AUTO_LINK_MARGIN: f64 = 0.20;

/// A retired-epic status set shared with relatedness (rollup.py re-exports
/// relatedness's set; one set here, same members).
pub fn is_retired_epic_status(status: Option<&str>) -> bool {
    super::relatedness::is_retired_epic_status(status)
}

/// Outcome of the rollup ladder for one node.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub kind: &'static str, // exempt | linked | suggest | orphan | team
    pub epic_id: Option<String>,
    pub score: f64,
    pub candidates: Vec<(String, f64, String)>,
    pub reason: String,
}

fn is_retired(epic: &Value) -> bool {
    is_retired_epic_status(epic.get("status").and_then(Value::as_str))
}

/// True iff walking `entry`'s parent chain reaches an epic. The full chain,
/// not the nest cap: a node that does reach a mission must never be reported
/// as an orphan. The seen set bounds a malformed parent cycle.
pub fn has_epic_ancestor(entry: &Value, id_to_entry: &BTreeMap<String, Value>) -> bool {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut current = entry
        .get("parent")
        .and_then(Value::as_str)
        .map(str::to_string);
    while let Some(c) = current {
        if !seen.insert(c.clone()) {
            break;
        }
        let Some(parent) = id_to_entry.get(&c) else {
            return false;
        };
        if parent.get("type").and_then(Value::as_str) == Some("epic") {
            return true;
        }
        current = parent
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    false
}

/// True iff this is OPEN feature/task work with no mission edge and no
/// opt-out. Exempt nodes (wrong type, a deliberate orphan_ok, or closed
/// work) are false, so every surface answers identically.
pub fn is_orphan(entry: &Value, id_to_entry: &BTreeMap<String, Value>) -> bool {
    if !ROLLUP_TYPES.contains(
        &entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ) {
        return false;
    }
    if entry
        .get("orphan_ok")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    if CLOSED_STATUSES.contains(
        &entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ) {
        return false;
    }
    !has_epic_ancestor(entry, id_to_entry)
}

/// The one epic a team scope names, or None. Only a scope that IS one live
/// epic can parent: a project portfolio names no node, and a multi-member
/// epic set names several.
pub fn team_epic_from_scope(scope: Option<&str>, entries: &[Value]) -> Option<String> {
    let scope = scope?;
    let members: Vec<&str> = scope
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if members.len() != 1 {
        return None;
    }
    for e in entries {
        if id(e) != members[0] {
            continue;
        }
        if e.get("type").and_then(Value::as_str) != Some("epic") || is_retired(e) {
            return None;
        }
        return Some(members[0].to_string());
    }
    None
}

fn id(e: &Value) -> &str {
    e.get("id").and_then(Value::as_str).unwrap_or_default()
}

/// The team outcome for a teamed filer's unlinked node, or None. The guess
/// rides `reason` and the caller prints the receipt, so the edge is never
/// silent. Nesting and cycle guards match the auto-link path.
fn team_resolution(node: &Value, entries: &[Value], crown_scope: &str) -> Option<Resolution> {
    let epic_id = team_epic_from_scope(Some(crown_scope), entries)?;
    let target = find_node(entries, &epic_id)?;
    if would_exceed_epic_depth(entries, node, &target) {
        return None;
    }
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or_default();
    if would_create_cycle(entries, node_id, target.get("id").and_then(Value::as_str)?) {
        return None;
    }
    Some(Resolution {
        kind: "team",
        epic_id: Some(target["id"].as_str().expect("target id").to_string()),
        score: 0.0,
        candidates: Vec::new(),
        reason: "filing session team scope".into(),
    })
}

/// This session's team scope, or None - never raises. The Python leg read
/// the same registry row through `current_team`.
fn current_team_scope() -> Option<String> {
    let get = |name: &str| std::env::var(name).ok();
    let ident = crate::spawn_context::resolve_self_identity(
        &get,
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let harness = ident.harness.as_deref()?.trim();
    let session_id = ident.session_id.as_deref()?.trim();
    if harness.is_empty() || session_id.is_empty() {
        return None;
    }
    let registry_path = crate::paths::AgentsHome::from_env().registry_json();
    let registry = crate::state::load_registry(&registry_path).ok()?;
    registry
        .find_by_session(harness, session_id)
        .and_then(|row| row.crown_scope.clone())
}

/// Run the rollup ladder for a node that already exists in `entries`.
/// Pure: scores and decides, never mutates. `crown_scope` overrides the
/// ambient registry read when the caller already holds one; None resolves
/// the caller's own team lazily on the suggest/orphan tail only.
pub fn resolve(node: &Value, entries: &[Value], crown_scope: Option<String>) -> Resolution {
    if !ROLLUP_TYPES.contains(&node.get("type").and_then(Value::as_str).unwrap_or_default())
        || node
            .get("orphan_ok")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Resolution {
            kind: "exempt",
            epic_id: None,
            score: 0.0,
            candidates: Vec::new(),
            reason: String::new(),
        };
    }
    // ANY explicit parent is the operator's answer to "what does this serve".
    // Rollup proposes an edge where none exists; it never overrules one a
    // human set, because the printed undo could not restore what it
    // overwrote.
    if node
        .get("parent")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty())
    {
        return exempt_with("parent already set");
    }

    let candidates = epic_candidates(node, entries, 3, None);
    if candidates.is_empty() {
        // "This serves no mission" is only advice worth giving when missions
        // exist to serve. On a graph with no live epic there is nothing to
        // link to, so the line would fire on every single intake.
        let any_live_epic = entries
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("epic") && !is_retired(e));
        if !any_live_epic {
            return exempt_with("no epics in graph");
        }
        return team_or_orphan(node, entries, crown_scope, candidates);
    }
    let top = candidates[0].clone();
    let runner_up = candidates.get(1).map(|c| c.1).unwrap_or(0.0);
    if top.1 >= AUTO_LINK_MIN && (top.1 - runner_up) >= AUTO_LINK_MARGIN {
        return Resolution {
            kind: "linked",
            epic_id: Some(top.0.clone()),
            score: top.1,
            candidates,
            reason: top.2,
        };
    }
    team_or_orphan(node, entries, crown_scope, candidates)
}

fn exempt_with(reason: &str) -> Resolution {
    Resolution {
        kind: "exempt",
        epic_id: None,
        score: 0.0,
        candidates: Vec::new(),
        reason: reason.to_string(),
    }
}

fn team_or_orphan(
    node: &Value,
    entries: &[Value],
    crown_scope: Option<String>,
    candidates: Vec<(String, f64, String)>,
) -> Resolution {
    let scope = match crown_scope {
        Some(scope) => Some(scope),
        None => current_team_scope(),
    };
    if let Some(scope) = scope {
        if !scope.is_empty() {
            if let Some(teamed) = team_resolution(node, entries, &scope) {
                return teamed;
            }
        }
    }
    if candidates.is_empty() {
        Resolution {
            kind: "orphan",
            epic_id: None,
            score: 0.0,
            candidates: Vec::new(),
            reason: String::new(),
        }
    } else {
        Resolution {
            kind: "suggest",
            epic_id: None,
            score: 0.0,
            candidates,
            reason: String::new(),
        }
    }
}

/// Operator-facing lines for a resolution. Empty for exempt. The receipt is
/// FIRST relative to the edge: an auto-link is only safe because a human
/// reads it and can undo it.
pub fn receipt_lines(resolution: &Resolution, node_id: &str, entries: &[Value]) -> Vec<String> {
    fn title(entries: &[Value], eid: &str) -> String {
        entries
            .iter()
            .find(|e| id(e) == eid)
            .and_then(|e| e.get("title"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| eid.to_string())
    }

    fn fmt(score: f64) -> String {
        format!("{score:.2}")
    }

    match resolution.kind {
        "linked" => {
            let eid = resolution.epic_id.as_deref().unwrap_or_default();
            vec![format!(
                "rollup: auto-linked {node_id} -> {eid} \"{}\" (score {}); undo: fno backlog update {node_id} --parent null",
                title(entries, eid),
                fmt(resolution.score)
            )]
        }
        "team" => {
            let eid = resolution.epic_id.as_deref().unwrap_or_default();
            vec![format!(
                "rollup: team-linked {node_id} -> {eid} \"{}\" (filing session team scope); undo: fno backlog update {node_id} --parent null",
                title(entries, eid)
            )]
        }
        "suggest" => {
            let mut lines = vec![format!("rollup: no clear mission edge for {node_id}; candidates:")];
            for (eid, score, _reason) in &resolution.candidates {
                lines.push(format!(
                    "  {}  {}  \"{}\"  -> fno backlog update {node_id} --parent {eid}",
                    fmt(*score),
                    eid,
                    title(entries, eid)
                ));
            }
            lines
        }
        "orphan" => vec![format!(
            "rollup: no mission edge (orphan); mark deliberate with: fno backlog update {node_id} --orphan-ok \"<reason>\""
        )],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests: the golden stderr lines pinned as Rust assertions (wave 8).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn epic(id: &str, title: &str, domain: &str, status: &str) -> Value {
        json!({"id": id, "title": title, "type": "epic", "status": status, "domain": domain})
    }

    #[test]
    fn linked_receipt_names_score_and_undo() {
        let epic_row = epic(
            "x-e0aa0001",
            "Deployment pipeline hardening",
            "code",
            "ready",
        );
        let node = json!({"id": "ab-new", "title": "Deployment pipeline hardening phase two",
                          "type": "feature", "domain": "code"});
        let entries = vec![epic_row, node.clone()];
        let r = resolve(&node, &entries, None);
        assert_eq!(r.kind, "linked");
        let lines = receipt_lines(&r, "ab-new", &entries);
        assert_eq!(
            lines[0],
            "rollup: auto-linked ab-new -> x-e0aa0001 \"Deployment pipeline hardening\" (score 0.70); undo: fno backlog update ab-new --parent null"
        );
    }

    #[test]
    fn suggest_receipt_lists_scored_candidates() {
        let epic_row = epic(
            "x-e0aa0001",
            "Deployment pipeline hardening",
            "code",
            "ready",
        );
        let node = json!({"id": "ab-new", "title": "Deployment pipeline observability short",
                          "type": "feature", "domain": "code"});
        let entries = vec![epic_row, node.clone()];
        let r = resolve(&node, &entries, None);
        assert_eq!(r.kind, "suggest", "{r:?}");
        let lines = receipt_lines(&r, "ab-new", &entries);
        assert!(
            lines[0].starts_with("rollup: no clear mission edge for ab-new; candidates:"),
            "{lines:?}"
        );
    }

    #[test]
    fn orphan_receipt_when_no_candidate_clears_the_floor() {
        let epic_row = epic(
            "x-e0aa0001",
            "Deployment pipeline hardening",
            "code",
            "ready",
        );
        let node = json!({"id": "ab-new", "title": "Quantum flux capacitor overhaul",
                          "type": "feature", "domain": "code"});
        let entries = vec![epic_row, node.clone()];
        let r = resolve(&node, &entries, None);
        assert_eq!(r.kind, "orphan");
        let lines = receipt_lines(&r, "ab-new", &entries);
        assert!(lines[0].contains("--orphan-ok"), "{lines:?}");
    }

    #[test]
    fn team_scope_links_a_single_live_epic() {
        let epic_row = epic(
            "x-epic0001",
            "A very distinctive mission name",
            "code",
            "ready",
        );
        let node = json!({"id": "ab-new", "title": "Something unrelated entirely", "type": "feature",
                          "domain": "code"});
        let entries = vec![epic_row, node.clone()];
        let r = resolve(&node, &entries, Some("x-epic0001".into()));
        assert_eq!(r.kind, "team");
        assert_eq!(r.epic_id.as_deref(), Some("x-epic0001"));
        let lines = receipt_lines(&r, "ab-new", &entries);
        assert!(lines[0].starts_with("rollup: team-linked"), "{lines:?}");
    }

    #[test]
    fn exempt_when_parent_set_or_bulk_or_no_epics() {
        let parented =
            json!({"id": "ab-x", "title": "T", "type": "feature", "parent": "x-epic0001"});
        let r = resolve(&parented, &[epic("x-epic0001", "E", "code", "ready")], None);
        assert_eq!(r.kind, "exempt");
        assert_eq!(r.reason, "parent already set");

        let bulk = json!({"id": "ab-y", "title": "T", "type": "roadmap"});
        let r = resolve(&bulk, &[epic("x-epic0001", "E", "code", "ready")], None);
        assert_eq!(r.kind, "exempt");

        // No live epic in the graph: exempt with the named reason.
        let done = epic("x-old", "Old mission", "code", "done");
        let node = json!({"id": "ab-z", "title": "T", "type": "feature"});
        let r = resolve(&node, &[done], None);
        assert_eq!(r.kind, "exempt");
        assert_eq!(r.reason, "no epics in graph");
    }

    #[test]
    fn is_orphan_matches_the_closed_and_optout_rules() {
        let index = |rows: &[Value]| -> BTreeMap<String, Value> {
            rows.iter()
                .map(|r| (id(r).to_string(), r.clone()))
                .collect()
        };
        let open = json!({"id": "ab-a", "title": "T", "type": "feature"});
        assert!(is_orphan(&open, &index(std::slice::from_ref(&open))));
        let closed = json!({"id": "ab-b", "title": "T", "type": "feature", "status": "done"});
        assert!(!is_orphan(&closed, &index(std::slice::from_ref(&closed))));
        let opted = json!({"id": "ab-c", "title": "T", "type": "feature", "orphan_ok": true});
        assert!(!is_orphan(&opted, &index(std::slice::from_ref(&opted))));
    }
}
