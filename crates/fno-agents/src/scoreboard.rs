//! The one delivery classifier behind every scoreboard view.
//!
//! One decision about what a node's evidence means, so the main, provider,
//! skill, efficiency, lane, calibration and fidelity views cannot drift into
//! seven answers. The terminal vocabulary arrives from the caller (Python owns
//! `fno.terminals`); the decision lives here:
//!
//! - a confirmed merge delivers the node, ledger row or not;
//! - an explicit doc/delivery terminal delivers the node with its evidence;
//! - a session terminal on a KNOWN node without a merge is never a delivery
//!   (an in-review node cannot ship from DonePRGreen);
//! - a session terminal on a node the graph lost stays a fallback, labeled
//!   `inferred`, never equal to a confirmed merge.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalVocabulary<'a> {
    /// Terminals that deliver a document outright (DoneAdvisory).
    pub doc: &'a [String],
    /// Terminals that record an explicit delivery outcome (DoneDelivery).
    pub delivery: &'a [String],
    /// Ship terminals that prove delivery only via a merge (DonePRGreen,
    /// DoneBatched) and otherwise are a session's last word, not evidence.
    pub ship: &'a [String],
}

impl<'a> TerminalVocabulary<'a> {
    fn class_of(&self, terminal: &str) -> Option<&'static str> {
        if self.doc.iter().any(|t| t == terminal) {
            Some("doc")
        } else if self.delivery.iter().any(|t| t == terminal) {
            Some("delivery")
        } else if self.ship.iter().any(|t| t == terminal) {
            Some("ship")
        } else {
            None
        }
    }
}

fn str_field<'a>(node: &'a Map<String, Value>, key: &'a str) -> Option<&'a str> {
    node.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn project_of(obj: &Map<String, Value>, project: &str) -> bool {
    obj.get("project")
        .and_then(Value::as_str)
        .map(|p| p == project)
        .unwrap_or(false)
}

/// Parse the ISO shapes the graph and ledger carry (naive local, or
/// Z-suffixed / offset UTC) to epoch seconds; None means "no trustworthy
/// time", which the caller treats as unknown, never as zero.
fn iso_secs(raw: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp());
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

/// The 14-day quality cohort over the classified nodes: a delivery survives
/// when its node is not reverted and no caused_by fix-node was created after
/// the ship within the follow-up window. Only deliveries that completed the
/// window are judged; younger ones are pending, never in the denominator.
/// Without causal telemetry it degrades to n/a.
fn survival(
    by_node: &Map<String, Value>,
    by_id: &BTreeMap<String, &Map<String, Value>>,
    now: Option<&str>,
    followup_days: i64,
) -> Value {
    let w4 = by_id
        .values()
        .any(|n| n.contains_key("reverted") || n.contains_key("caused_by"));
    if !w4 {
        return json!({"available": false, "reason": "no causal telemetry (Wave 4 not shipped)"});
    }
    if !by_node.values().any(|c| c["delivered"] == true) {
        return json!({"available": false, "reason": "no shipped nodes in window"});
    }
    let Some(now_secs) = now.and_then(iso_secs) else {
        return json!({"available": false, "reason": "no usable now"});
    };
    let window = followup_days * 86400;
    let mut mature: Vec<(&String, &Map<String, Value>, i64)> = Vec::new();
    for (nid, c) in by_node {
        if c["delivered"] != true {
            continue;
        }
        let Some(ts) = c["ship_ts"].as_str().and_then(iso_secs) else {
            continue;
        };
        if now_secs - ts >= window {
            if let Some(node) = by_id.get(nid) {
                mature.push((nid, *node, ts));
            }
        }
    }
    let delivered = by_node.values().filter(|c| c["delivered"] == true).count();
    let pending = delivered - mature.len();
    let mut survived = 0i64;
    for (nid, node, shipped_at) in &mature {
        if node.get("reverted") == Some(&Value::Bool(true)) {
            continue;
        }
        let mut followed = false;
        for fix in by_id.values() {
            let Some(origin) = fix.get("caused_by").and_then(Value::as_str) else {
                continue;
            };
            if origin != *nid {
                continue;
            }
            match fix
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(iso_secs)
            {
                // Unparsable fix time: stay conservative, it counts against
                // survival.
                None => {
                    followed = true;
                    break;
                }
                // A follow-up is a fix created AFTER the ship, within the
                // window. A fix predating the ship or post-window is not a
                // follow-up to it.
                Some(fx) if fx >= *shipped_at && fx - *shipped_at <= window => {
                    followed = true;
                    break;
                }
                Some(_) => {}
            }
        }
        if !followed {
            survived += 1;
        }
    }
    json!({
        "available": true,
        "survived": survived,
        "shipped_nodes": mature.len(),
        // Rounded like the _pct the Python fold used before this moved here.
        "rate_pct": if mature.is_empty() {
            0
        } else {
            ((100 * survived) as f64 / mature.len() as f64).round() as i64
        },
        "pending": pending,
    })
}

/// Classify one node from its graph row and its ledger rows.
/// The returned shape is the wire contract the Python views consume.
pub fn classify_node(
    node: Option<&Map<String, Value>>,
    node_id: &str,
    rows: &[&Map<String, Value>],
    vocab: &TerminalVocabulary,
) -> Value {
    let mut best_terminal: Option<(&'static str, &str)> = None;
    let mut ship_ts: Option<&str> = None;
    let mut cost = 0.0f64;
    let mut cost_known = false;
    for row in rows {
        if let Some(terminal) = str_field(row, "termination_reason") {
            let kind = vocab.class_of(terminal);
            if let Some(kind) = kind {
                if best_terminal.is_none() {
                    best_terminal = Some((kind, terminal));
                }
                if let Some(ts) = str_field(row, "completed") {
                    ship_ts = Some(ts);
                }
            }
        }
        if let Some(c) = row.get("cost_usd").and_then(Value::as_f64) {
            if c.is_finite() && c >= 0.0 {
                cost += c;
                cost_known = true;
            }
        }
    }

    let node_ship_ts =
        node.and_then(|n| str_field(n, "merged_at").or_else(|| str_field(n, "completed_at")));
    // A merge timestamp is merge evidence even when the status field is stale
    // or missing: current confirmed evidence wins over an old state value.
    let merged = node.is_some_and(|n| {
        str_field(n, "merge_status") == Some("merged") || str_field(n, "merged_at").is_some()
    });

    let (class, delivered, confirmed, evidence) = if merged {
        ("merged", true, true, "graph_merge")
    } else if let Some(("doc", _)) = best_terminal {
        ("delivered_doc", true, true, "doc_terminal")
    } else if let Some(("delivery", _)) = best_terminal {
        ("delivered_delivery", true, true, "delivery_terminal")
    } else if node.is_some() && best_terminal.is_some() {
        // Known node, no merge, but a session claims it shipped: the stop is
        // recorded as evidence and nothing more. It can never promote to a
        // delivery while the graph says unmerged.
        ("unmerged", false, false, "session_terminal")
    } else if node.is_none() && best_terminal.is_some() {
        // The graph lost this node; the terminal is all that is left. Keep it,
        // labeled, so the count survives without passing as a confirmed merge.
        ("inferred", true, false, "session_terminal")
    } else {
        ("no_evidence", false, false, "none")
    };

    json!({
        "node_id": node_id,
        "class": class,
        "delivered": delivered,
        "confirmed": confirmed,
        "evidence": evidence,
        "ship_ts": node_ship_ts.or(ship_ts),
        "node_known": node.is_some(),
        "cost_usd": cost,
        "cost_known": cost_known,
        "rows": rows.len(),
    })
}

/// Classify every node in `entries` plus every row-referenced node id the
/// graph does not carry. Params: `entries` (graph nodes), `rows` (ledger
/// rows), `doc_terminals` / `delivery_terminals` / `ship_terminals`.
/// Returns `{"by_node": {...}, "coverage": {...}}`. Pure; no file I/O.
pub fn classify(params: &Value) -> Result<Value, String> {
    let empty = Vec::new();
    let all_entries = params
        .get("entries")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let all_rows = params
        .get("rows")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // Optional project scope: the denominator is scoped once, here, so no
    // view re-filters. Rows with no project stay unattributed - counted in
    // the scope, never copied into the project.
    let project = params
        .get("project")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let mut scope: Option<Value> = None;
    let mut node_ids: Vec<String> = Vec::new();
    let (entries, rows): (Vec<Value>, Vec<Value>) = match project {
        Some(project) => {
            let of = |v: &Value, key: &str| -> bool {
                v.get(key)
                    .and_then(Value::as_str)
                    .map(|p| p == project)
                    .unwrap_or(false)
            };
            let of_obj = |n: &Map<String, Value>| project_of(n, project);
            node_ids = all_entries
                .iter()
                .filter_map(Value::as_object)
                .filter(|n| of_obj(n))
                .filter_map(|n| str_field(n, "id"))
                .map(str::to_string)
                .collect();
            let entries: Vec<Value> = all_entries
                .iter()
                .filter(|n| n.as_object().map(of_obj).unwrap_or(false))
                .cloned()
                .collect();
            let rows: Vec<Value> = all_rows
                .iter()
                .filter(|r| r.as_object().map(of_obj).unwrap_or(false))
                .cloned()
                .collect();
            let unattributed = all_rows
                .iter()
                .filter(|r| {
                    r.get("project")
                        .and_then(Value::as_str)
                        .map(str::is_empty)
                        .unwrap_or(true)
                })
                .count();
            let in_project = rows.len();
            scope = Some(json!({
                "project": project,
                "nodes": node_ids.len(),
                "unattributed_rows": unattributed,
                "other_project_rows": all_rows.len() - in_project - unattributed,
            }));
            (entries, rows)
        }
        None => (
            all_entries.clone(),
            all_rows.iter().filter(|r| r.is_object()).cloned().collect(),
        ),
    };
    let list = |key: &str| -> Vec<String> {
        params
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let doc = list("doc_terminals");
    let delivery = list("delivery_terminals");
    let ship = list("ship_terminals");
    let vocab = TerminalVocabulary {
        doc: &doc,
        delivery: &delivery,
        ship: &ship,
    };

    let mut by_id: BTreeMap<String, &Map<String, Value>> = BTreeMap::new();
    for entry in &entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(id) = str_field(obj, "id") else {
            continue;
        };
        by_id.insert(id.to_string(), obj);
    }

    let mut rows_by_node: BTreeMap<String, Vec<&Map<String, Value>>> = BTreeMap::new();
    let mut rowless: Vec<&Map<String, Value>> = Vec::new();
    for row in &rows {
        let Some(obj) = row.as_object() else {
            continue;
        };
        match str_field(obj, "graph_node_id") {
            Some(nid) => rows_by_node.entry(nid.to_string()).or_default().push(obj),
            None => rowless.push(obj),
        }
    }

    let mut by_node = Map::new();
    for (id, node) in &by_id {
        let node_rows: Vec<&Map<String, Value>> = rows_by_node.remove(id).unwrap_or_default();
        by_node.insert(
            id.clone(),
            classify_node(Some(node), id, &node_rows, &vocab),
        );
    }
    // Row-referenced ids the graph does not carry: the inferred population.
    for (id, node_rows) in &rows_by_node {
        by_node.insert(id.clone(), classify_node(None, id, node_rows, &vocab));
    }

    let inferred = by_node
        .values()
        .filter(|c| c["class"] == "inferred")
        .count();
    let survival = survival(
        &by_node,
        &by_id,
        params.get("now").and_then(Value::as_str),
        params
            .get("followup_days")
            .and_then(Value::as_i64)
            .unwrap_or(14),
    );
    let mut result = json!({
        "by_node": by_node,
        "coverage": {
            "nodes": by_id.len(),
            "rows_with_node": rows.len() - rowless.len(),
            "rows_without_node": rowless.len(),
            "inferred_nodes": inferred,
        },
        "survival": survival,
    });
    if let Some(scope) = scope {
        result["coverage"]["project_scope"] = scope.clone();
        result["scoped"] = json!({
            "entries": entries,
            "rows": rows,
            "node_ids": node_ids,
            "scope": scope,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One vocabulary builder shared by every test in this module. The String
    /// vectors live in a local binding so the borrows stay valid for the call.
    fn with_vocab<F: FnOnce(&TerminalVocabulary)>(f: F) {
        let doc = vec!["DoneAdvisory".to_string()];
        let delivery = vec!["DoneDelivery".to_string()];
        let ship = vec!["DonePRGreen".to_string(), "DoneBatched".to_string()];
        f(&TerminalVocabulary {
            doc: &doc,
            delivery: &delivery,
            ship: &ship,
        })
    }

    fn row(node: &str, terminal: &str) -> Value {
        json!({"graph_node_id": node, "termination_reason": terminal, "cost_usd": 1.5})
    }

    fn node(id: &str, merge_status: Option<&str>) -> Value {
        json!({"id": id, "merge_status": merge_status})
    }

    fn one(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn merge_delivers_without_any_ledger_row() {
        with_vocab(|v| {
            let n = one(node("x-1", Some("merged")));
            let out = classify_node(Some(&n), "x-1", &[], v);
            assert_eq!(out["class"], "merged");
            assert_eq!(out["delivered"], true);
            assert_eq!(out["confirmed"], true);
            assert_eq!(out["cost_known"], false);
        });
    }

    #[test]
    fn in_review_node_cannot_ship_from_done_terminal() {
        with_vocab(|v| {
            let n = one(node("x-1", None));
            let r = one(row("x-1", "DonePRGreen"));
            let out = classify_node(Some(&n), "x-1", &[&r], v);
            assert_eq!(out["class"], "unmerged");
            assert_eq!(out["delivered"], false);
            assert_eq!(out["evidence"], "session_terminal");
        });
    }

    #[test]
    fn advisory_terminal_delivers_a_document() {
        with_vocab(|v| {
            let n = one(node("d-1", None));
            let r = one(row("d-1", "DoneAdvisory"));
            let out = classify_node(Some(&n), "d-1", &[&r], v);
            assert_eq!(out["class"], "delivered_doc");
            assert_eq!(out["delivered"], true);
            assert_eq!(out["evidence"], "doc_terminal");
        });
    }

    #[test]
    fn delivery_terminal_preserves_its_evidence() {
        with_vocab(|v| {
            let n = one(node("x-1", None));
            let r = one(row("x-1", "DoneDelivery"));
            let out = classify_node(Some(&n), "x-1", &[&r], v);
            assert_eq!(out["class"], "delivered_delivery");
            assert_eq!(out["delivered"], true);
        });
    }

    #[test]
    fn lost_node_ships_only_as_inferred() {
        with_vocab(|v| {
            let r = one(row("x-lost", "DonePRGreen"));
            let out = classify_node(None, "x-lost", &[&r], v);
            assert_eq!(out["class"], "inferred");
            assert_eq!(out["delivered"], true);
            assert_eq!(out["confirmed"], false);
        });
    }

    #[test]
    fn merge_outranks_a_stale_unmerged_row_history() {
        with_vocab(|v| {
            let mut n = one(node("x-1", Some("merged")));
            n.insert("merged_at".into(), json!("2026-09-10T00:00:00Z"));
            let r = one(row("x-1", "NoProgress"));
            let out = classify_node(Some(&n), "x-1", &[&r], v);
            assert_eq!(out["class"], "merged");
            assert_eq!(out["ship_ts"], "2026-09-10T00:00:00Z");
        });
    }

    #[test]
    fn a_merge_timestamp_wins_over_a_stale_status_value() {
        with_vocab(|v| {
            let n = one(
                json!({"id": "x-1", "merge_status": "queued", "merged_at": "2026-09-10T00:00:00Z"}),
            );
            let r = one(row("x-1", "DonePRGreen"));
            let out = classify_node(Some(&n), "x-1", &[&r], v);
            assert_eq!(out["class"], "merged");
            assert_eq!(out["confirmed"], true);
        });
    }

    #[test]
    fn survival_judges_only_mature_deliveries() {
        let params = json!({
            "entries": [
                {"id": "x-old", "merge_status": "merged", "completed_at": "2026-09-01T10:00:00", "reverted": false},
                {"id": "x-new", "merge_status": "merged", "completed_at": "2026-09-20T10:00:00", "reverted": false},
                {"id": "x-fix", "caused_by": "x-old", "created_at": "2026-09-10T00:00:00"}
            ],
            "rows": [
                {"graph_node_id": "x-old", "termination_reason": "DonePRGreen", "completed": "2026-09-01T10:00:00"},
                {"graph_node_id": "x-new", "termination_reason": "DonePRGreen", "completed": "2026-09-20T10:00:00"}
            ],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
            "now": "2026-09-21T10:00:00",
            "followup_days": 14
        });
        let out = classify(&params).unwrap();
        let su = &out["survival"];
        assert_eq!(su["available"], true);
        // x-old is 20 days old: mature. x-new is 1 day old: pending.
        assert_eq!(su["shipped_nodes"], 1);
        assert_eq!(su["survived"], 0);
        assert_eq!(su["survived"], 0); // x-fix lands 9 days after the ship: followed
        assert_eq!(su["pending"], 1);
    }

    #[test]
    fn project_scope_filters_and_reports_unattributed() {
        let params = json!({
            "entries": [
                {"id": "x-1", "project": "p1", "merge_status": "merged"},
                {"id": "x-2", "project": "t2", "merge_status": "merged"}
            ],
            "rows": [
                {"graph_node_id": "x-1", "termination_reason": "DonePRGreen", "cost_usd": 1.0, "project": "p1"},
                {"graph_node_id": "x-2", "termination_reason": "DonePRGreen", "cost_usd": 9.0, "project": "p2"},
                {"completed": "2026-09-10T00:00:00Z", "termination_reason": "DonePRGreen", "cost_usd": 5.0}
            ],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
            "project": "p1"
        });
        let out = classify(&params).unwrap();
        assert_eq!(out["coverage"]["project_scope"]["project"], "p1");
        assert_eq!(out["coverage"]["project_scope"]["unattributed_rows"], 1);
        assert_eq!(out["coverage"]["project_scope"]["other_project_rows"], 1);
        let scoped = &out["scoped"];
        assert_eq!(scoped["entries"].as_array().unwrap().len(), 1);
        assert_eq!(scoped["rows"].as_array().unwrap().len(), 1);
        assert_eq!(scoped["node_ids"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_cost_reads_as_unknown_never_zero() {
        with_vocab(|v| {
            let n = one(node("x-1", Some("merged")));
            let mut r = one(row("x-1", "DonePRGreen"));
            r.insert("cost_usd".into(), json!("not-a-number"));
            let out = classify_node(Some(&n), "x-1", &[&r], v);
            assert_eq!(out["cost_known"], false);
        });
    }

    #[test]
    fn classify_separates_known_inferred_and_nodeless() {
        let params = json!({
            "entries": [node("x-1", Some("merged")), node("x-2", None)],
            "rows": [
                row("x-1", "DonePRGreen"),
                row("x-lost", "DonePRGreen"),
                {"completed": "2026-09-10T00:00:00Z", "termination_reason": "DonePRGreen"},
                {"completed": "2026-09-10T00:00:00Z", "termination_reason": "Budget", "graph_node_id": "x-2"},
            ],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
        });
        let out = classify(&params).unwrap();
        assert_eq!(out["by_node"]["x-1"]["class"], "merged");
        assert_eq!(out["by_node"]["x-2"]["class"], "no_evidence");
        assert_eq!(out["by_node"]["x-lost"]["class"], "inferred");
        assert_eq!(out["coverage"]["rows_without_node"], 1);
        assert_eq!(out["coverage"]["inferred_nodes"], 1);
    }
}
