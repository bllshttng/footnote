//! Supersession verification and blocked_by edge settlement summaries: the
//! pure halves of the reconcile sweep's supersession leg, ported from
//! graph/_reconcile.py. The store-side settlement lives in graph_store
//! (`settle_blocked_by_edges`); this module owns the cause-surface proof a
//! merged PR's changed-file set owes its predecessors, and the receipt
//! rendering both verbs print.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One spelling for a repo-relative path on both sides of the match.
///
/// `strip_prefix` and not a trim of the `"./"` character set: a set-trim
/// eats the leading dot of every dotfile path and lets a declared
/// `github/ci.yml` falsely match a changed `.github/ci.yml`.
pub(crate) fn normalize_surface(path: &str) -> String {
    let replaced = path.trim().replace('\\', "/");
    replaced.strip_prefix("./").unwrap_or(&replaced).to_string()
}

/// A PR URL reduced to its comparable form, or None.
///
/// Query, fragment and a trailing slash are display noise; the graph and gh
/// can differ on all three for the same PR. Lowercased because the host and
/// owner segments are case-insensitive in practice and the path segments
/// the comparison relies on are already lowercase.
pub(crate) fn normalized_pr_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    let mut stripped = url.trim();
    for sep in ['?', '#'] {
        stripped = stripped.split(sep).next().unwrap_or(stripped);
    }
    let stripped = stripped.trim_end_matches('/');
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_lowercase())
    }
}

/// Verify predecessor cause surfaces against one merged PR's file set.
///
/// Full contract: docs/architecture/backlog-graph-verb-contracts.md
pub(crate) fn verify_pending_supersessions(
    entries: &mut [Value],
    successor: &str,
    changed_files: &[String],
    evidence_pr: i64,
    verified_at: Option<&str>,
    evidence_complete: bool,
) -> Vec<Value> {
    let changed: std::collections::BTreeSet<String> = changed_files
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| normalize_surface(p))
        .collect();
    let stamp = verified_at
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    let mut receipts: Vec<Value> = Vec::new();
    for entry in entries.iter_mut() {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if entry.get("superseded_by").and_then(Value::as_str) != Some(successor) {
            continue;
        }
        // A verified record never re-verifies.
        if entry
            .get("supersession")
            .map(|r| r.get("verified_at").is_some())
            .unwrap_or(false)
        {
            continue;
        }
        let surfaces: Vec<String> = entry
            .get("supersession")
            .and_then(|r| r.get("surfaces"))
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(normalize_surface)
                    .collect()
            })
            .unwrap_or_default();
        let matched: Vec<&String> = surfaces.iter().filter(|s| changed.contains(*s)).collect();
        let uncovered: Vec<&String> = surfaces.iter().filter(|s| !changed.contains(*s)).collect();
        if uncovered.is_empty() {
            if let Some(record) = entry.get_mut("supersession").and_then(Value::as_object_mut) {
                record.insert("verified_at".into(), json!(stamp));
                record.insert("evidence_pr".into(), json!(evidence_pr));
                record.insert(
                    "matched_surfaces".into(),
                    json!(matched.into_iter().cloned().collect::<Vec<String>>()),
                );
            }
            continue;
        }
        // Uncovered surfaces: held. Truncated evidence names the truncation;
        // complete evidence names the real gap.
        let kind = if evidence_complete {
            "supersession_unverified"
        } else {
            "supersession_evidence_truncated"
        };
        let cause = entry
            .get("supersession")
            .and_then(|r| r.get("cause"))
            .cloned()
            .unwrap_or(Value::Null);
        receipts.push(json!({
            "kind": kind,
            "predecessor": id,
            "successor": successor,
            "cause": cause,
            "uncovered_surfaces": uncovered.into_iter().cloned().collect::<Vec<String>>(),
            "evidence_pr": evidence_pr,
        }));
    }
    receipts
}

/// Successor id -> successor node (cloned), for pending predecessors
/// already owed proof. Only a CLOSED successor is owed here: an open one
/// still has its ordinary close ahead of it, and that path verifies.
pub(crate) fn successors_owing_verification(entries: &[Value]) -> BTreeMap<String, Value> {
    let by_id: BTreeMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();
    let mut owed = BTreeMap::new();
    for entry in entries {
        let Some(successor_id) = entry.get("superseded_by").and_then(Value::as_str) else {
            continue;
        };
        let Some(record) = entry.get("supersession") else {
            continue;
        };
        if record.get("verified_at").is_some() {
            continue;
        }
        let Some(successor) = by_id.get(successor_id) else {
            continue;
        };
        let closed = successor
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let has_pr = successor
            .get("pr_number")
            .map(Value::is_i64)
            .unwrap_or(false);
        if closed && has_pr {
            owed.insert(successor_id.to_string(), (*successor).clone());
        }
    }
    owed
}

/// Apply the settlement's `blocked_by` map to rows in place; receipts out.
pub(crate) fn apply_edge_settlement(entries: &mut [Value], settlement: &Value) -> Vec<Value> {
    let Some(map) = settlement.get("blocked_by").and_then(Value::as_object) else {
        return Vec::new();
    };
    for node in entries.iter_mut() {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(blockers) = map.get(id) {
            if let Some(obj) = node.as_object_mut() {
                obj.insert("blocked_by".into(), blockers.clone());
            }
        }
    }
    settlement
        .get("receipts")
        .and_then(Value::as_array)
        .map(|rows| rows.clone())
        .unwrap_or_default()
}

/// One line for the sweep report; held edges name why they stay.
pub(crate) fn summarize_edge_settlement(receipts: &[Value]) -> String {
    let count = |kind: &str| {
        receipts
            .iter()
            .filter(|r| r.get("kind").and_then(Value::as_str) == Some(kind))
            .count()
    };
    format!(
        "blocked_by edges settled: {} pruned, {} rewired, {} held (a deferred or missing blocker stays)",
        count("blocked_by_pruned"),
        count("blocked_by_rewired"),
        count("blocked_by_held"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(id: &str, surfaces: &[&str], verified: bool) -> Value {
        let mut record = json!({"cause": "dup of the legend work", "surfaces": surfaces});
        if verified {
            record["verified_at"] = json!("2026-09-01T00:00:00Z");
        }
        json!({
            "id": id,
            "superseded_by": "x-succ",
            "supersession": record,
        })
    }

    #[test]
    fn normalize_keeps_a_dotfile_prefix() {
        assert_eq!(normalize_surface(".github/ci.yml"), ".github/ci.yml");
        assert_eq!(normalize_surface("./github/ci.yml"), "github/ci.yml");
        assert_eq!(normalize_surface(" a\\b.rs "), "a/b.rs");
    }

    #[test]
    fn normalized_pr_url_strips_display_noise() {
        assert_eq!(
            normalized_pr_url(Some("https://GitHub.com/O/R/pull/7?diff=split#issue")),
            Some("https://github.com/o/r/pull/7".into())
        );
        assert_eq!(
            normalized_pr_url(Some("https://x.y/pull/7/")),
            Some("https://x.y/pull/7".into())
        );
        assert_eq!(normalized_pr_url(Some("  ")), None);
        assert_eq!(normalized_pr_url(None), None);
    }

    #[test]
    fn full_surface_match_stamps_the_record() {
        let mut entries = vec![pred("x-pre", &["src/a.rs", "./src/b.rs"], false)];
        let receipts = verify_pending_supersessions(
            &mut entries,
            "x-succ",
            &["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
            77,
            Some("2026-10-02T00:00:00Z"),
            true,
        );
        assert!(receipts.is_empty());
        let record = &entries[0]["supersession"];
        assert_eq!(record["verified_at"], "2026-10-02T00:00:00Z");
        assert_eq!(record["evidence_pr"], 77);
        assert_eq!(record["matched_surfaces"], json!(["src/a.rs", "src/b.rs"]));
    }

    #[test]
    fn uncovered_surfaces_hold_and_the_kind_tracks_completeness() {
        for (complete, kind) in [
            (true, "supersession_unverified"),
            (false, "supersession_evidence_truncated"),
        ] {
            let mut entries = vec![pred("x-pre", &["src/a.rs", "src/gone.rs"], false)];
            let receipts = verify_pending_supersessions(
                &mut entries,
                "x-succ",
                &["src/a.rs".into()],
                77,
                None,
                complete,
            );
            assert_eq!(receipts.len(), 1, "{kind}");
            assert_eq!(receipts[0]["kind"], kind);
            assert_eq!(receipts[0]["predecessor"], "x-pre");
            assert_eq!(receipts[0]["uncovered_surfaces"], json!(["src/gone.rs"]));
            assert!(entries[0]["supersession"].get("verified_at").is_none());
        }
    }

    #[test]
    fn verified_records_and_other_successors_are_skipped() {
        let mut entries = vec![
            pred("x-done", &["src/gone.rs"], true),
            pred("x-other", &["src/gone.rs"], false),
            json!({"id": "x-plain"}),
        ];
        let receipts = verify_pending_supersessions(&mut entries, "x-succ", &[], 1, None, true);
        assert!(receipts.is_empty(), "{receipts:?}");
        assert!(entries[1]["supersession"].get("verified_at").is_none());
    }

    #[test]
    fn only_a_closed_pr_carrying_successor_is_owed() {
        let entries = vec![
            pred("x-pre", &["s"], false),
            json!({"id": "x-succ", "completed_at": "2026-10-01T00:00:00Z", "pr_number": 12}),
            json!({"id": "x-succ-open", "pr_number": 13}),
        ];
        // The owed map keys off superseded_by: x-pre's successor is x-succ.
        let owed = successors_owing_verification(&entries);
        assert_eq!(owed.len(), 1);
        assert_eq!(owed["x-succ"]["pr_number"], 12);
    }

    #[test]
    fn settlement_applies_and_summarizes() {
        let mut entries = vec![
            json!({"id": "x-a", "blocked_by": ["x-done"]}),
            json!({"id": "x-b", "blocked_by": ["x-live"]}),
        ];
        let settlement = json!({
            "blocked_by": {"x-a": []},
            "receipts": [
                {"kind": "blocked_by_pruned"},
                {"kind": "blocked_by_rewired"},
                {"kind": "blocked_by_held"},
                {"kind": "blocked_by_held"},
            ],
        });
        let receipts = apply_edge_settlement(&mut entries, &settlement);
        assert_eq!(entries[0]["blocked_by"], json!([]));
        assert_eq!(entries[1]["blocked_by"], json!(["x-live"]));
        assert_eq!(receipts.len(), 4);
        assert_eq!(
            summarize_edge_settlement(&receipts),
            "blocked_by edges settled: 1 pruned, 1 rewired, 2 held (a deferred or missing blocker stays)"
        );
    }
}
