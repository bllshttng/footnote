//! The one delivery classifier behind every scoreboard view.
//!
//! One decision about what a node's evidence means, so the main, provider,
//! skill, efficiency, lane, calibration and fidelity views cannot drift into
//! seven answers. The terminal vocabulary arrives from the caller (Python owns
//! `fno.terminals`); the decision lives here. The `flow` section rides the
//! same answer: the weekly delivery/cycle/waiting aggregates every board and
//! view reads (x-b07a), so presentation layers never re-classify.
//!
//! - a confirmed merge delivers the node, ledger row or not;
//! - an explicit doc/delivery terminal delivers the node with its evidence;
//! - a session terminal on a KNOWN node without a merge is never a delivery
//!   (an in-review node cannot ship from DonePRGreen);
//! - a session terminal on a node the graph lost stays a fallback, labeled
//!   `inferred`, never equal to a confirmed merge.

use chrono::{Datelike, TimeZone};
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
/// time", which the caller treats as unknown, never as zero. A naive
/// timestamp is LOCAL wall time, like the ledger's `completed`, so it is
/// resolved in the machine zone; a DST-ambiguous or absent local time has
/// no trustworthy instant and reads as None.
fn iso_secs(raw: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp());
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .and_then(|dt| chrono::TimeZone::from_local_datetime(&chrono::Local, &dt).single())
        .map(|dt| dt.timestamp())
}

fn local_dt(secs: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::Local.timestamp_opt(secs, 0).single()
}

/// Monday of the local calendar week containing `secs`, as "YYYY-MM-DD".
fn week_start_of(secs: i64) -> Option<String> {
    let dt = local_dt(secs)?;
    Some(
        (dt.date_naive() - chrono::Duration::days(dt.weekday().num_days_from_monday() as i64))
            .format("%Y-%m-%d")
            .to_string(),
    )
}

/// Epoch seconds of local midnight on the Monday of the week holding `secs`.
/// DST shifts make this ±1h around the transition; the partial-week flags it
/// feeds tolerate that.
fn week_start_secs(secs: i64) -> Option<i64> {
    let dt = local_dt(secs)?;
    let monday =
        dt.date_naive() - chrono::Duration::days(dt.weekday().num_days_from_monday() as i64);
    chrono::Local
        .from_local_datetime(&monday.and_hms_opt(0, 0, 0)?)
        .single()
        .map(|dt| dt.timestamp())
}

fn has_pr(obj: &Map<String, Value>) -> bool {
    match obj.get("pr_number") {
        Some(Value::Number(n)) => n.as_i64().is_some(),
        Some(Value::String(s)) => !s.is_empty(),
        _ => false,
    }
}

/// Oldest age in whole days from `created_at` to now over the given entries;
/// null when no entry carries a usable created_at. Ages name their basis in
/// the payload (`age_basis`): node creation, never a touched_at inference.
fn oldest_age_days<'a>(
    entries: impl Iterator<Item = &'a Map<String, Value>>,
    now_secs: i64,
) -> Value {
    let oldest = entries
        .filter_map(|n: &Map<String, Value>| {
            n.get("created_at")
                .and_then(Value::as_str)
                .and_then(iso_secs)
        })
        .filter_map(|created| now_secs.checked_sub(created))
        .filter(|age| *age >= 0)
        .max()
        .map(|age| age / 86400);
    match oldest {
        Some(days) => json!(days),
        None => Value::Null,
    }
}

/// The weekly delivery flow (x-b07a) over the scoped entries/rows and the
/// classification they already received: merged PRs per local calendar week,
/// code/document deliveries separately, PR created-to-merged median and
/// nearest-rank p85 with sample count, current open-PR age, and the canonical
/// WIP/review/blocked populations with oldest ages. Aggregates only, so the
/// payload can ride the public boards; never estimates interval durations
/// from snapshots or touched_at. Confirmed deliveries bucket weekly; a
/// session-terminal delivery on a node the graph lost, and a delivered PR row
/// with no node, ride `coverage.unlinked` - counted in the repository
/// measure's coverage, never passed off as confirmed work.
fn flow(
    by_id: &BTreeMap<String, &Map<String, Value>>,
    rows: &[Value],
    by_node: &Map<String, Value>,
    now_raw: Option<&str>,
    since_days: i64,
    vocab_terms: &[&str],
) -> Value {
    let Some(now_secs) = now_raw.and_then(iso_secs) else {
        return json!({"available": false, "reason": "no usable now"});
    };
    if since_days <= 0 {
        return json!({"available": false, "reason": "non-positive window"});
    }
    let window_start = now_secs - since_days * 86400;

    // Enumerate the local calendar weeks the window touches, oldest first.
    let Some(first_week) = week_start_secs(window_start) else {
        return json!({"available": false, "reason": "no usable window start"});
    };
    let current_week = match week_start_secs(now_secs) {
        Some(w) => w,
        None => return json!({"available": false, "reason": "no usable now"}),
    };
    let mut buckets: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut week_rows: Vec<(String, i64, bool)> = Vec::new();
    let (Some(week_start_d), Some(current_monday_d)) = (
        local_dt(first_week).map(|dt| dt.date_naive()),
        local_dt(current_week).map(|dt| dt.date_naive()),
    ) else {
        return json!({"available": false, "reason": "no usable window start"});
    };
    let mut wd = week_start_d;
    while wd <= current_monday_d {
        let label = wd.format("%Y-%m-%d").to_string();
        let Some(naive) = wd.and_hms_opt(0, 0, 0) else {
            break;
        };
        let wsecs = match chrono::Local.from_local_datetime(&naive).single() {
            Some(dt) => dt.timestamp(),
            None => break,
        };
        let partial = window_start > wsecs || now_secs < wsecs + 7 * 86400;
        week_rows.push((label.clone(), 0, partial));
        buckets.insert(label, (0, 0));
        wd += chrono::Duration::weeks(1);
    }

    // Weekly buckets count CONFIRMED deliveries only: merged, explicit
    // delivery, and doc. Class decides the column, ship_ts decides the week.
    let mut code = 0i64;
    let mut doc = 0i64;
    let mut cycle_days: Vec<f64> = Vec::new();
    for (nid, c) in by_node {
        if c["delivered"] != true || c["confirmed"] != true {
            continue;
        }
        let Some(ts) = c["ship_ts"].as_str().and_then(iso_secs) else {
            continue;
        };
        let is_doc = c["class"] == "delivered_doc";
        if let Some(bucket) = week_start_of(ts).and_then(|w| buckets.get_mut(&w)) {
            if is_doc {
                bucket.1 += 1;
            } else {
                bucket.0 += 1;
            }
        }
        if is_doc {
            doc += 1;
        } else {
            code += 1;
        }
        if c["class"] == "merged" {
            if let (Some(created), Some(merged)) = (
                by_id
                    .get(nid)
                    .and_then(|n| n.get("created_at"))
                    .and_then(Value::as_str)
                    .and_then(iso_secs),
                c["ship_ts"].as_str().and_then(iso_secs),
            ) {
                let elapsed = (merged - created) as f64 / 86400.0;
                if elapsed >= 0.0 {
                    cycle_days.push(elapsed);
                }
            }
        }
    }

    // Waiting populations: the canonical graph statuses, current counts and
    // oldest ages. Accumulated blocked time is deliberately absent - the
    // graph carries no interval history, and touched_at is not one.
    let waiting_for = |status: &str| -> Value {
        let matched: Vec<&Map<String, Value>> = by_id
            .values()
            .filter(|n| str_field(n, "status") == Some(status))
            .copied()
            .collect();
        json!({
            "count": matched.len(),
            "oldest_age_days": oldest_age_days(matched.into_iter(), now_secs),
        })
    };

    // Unlinked coverage: a delivered session terminal on a node the graph
    // lost, and a delivered PR row with no node. Counted, never folded into
    // the confirmed weekly series.
    let unlinked_inferred = by_node
        .values()
        .filter(|c| c["delivered"] == true && c["node_known"] == false)
        .count() as i64;
    let mut unlinked_rows = 0i64;
    for row in rows {
        let Some(obj) = row.as_object() else {
            continue;
        };
        if str_field(obj, "graph_node_id").is_some() || !has_pr(obj) {
            continue;
        }
        let Some(tr) = str_field(obj, "termination_reason") else {
            continue;
        };
        if !vocab_terms.contains(&tr) {
            continue;
        }
        if str_field(obj, "completed").and_then(iso_secs).is_some() {
            unlinked_rows += 1;
        }
    }

    let cycle = if cycle_days.is_empty() {
        json!({"available": false, "reason": "no merged PR with created_at and merged_at in window"})
    } else {
        cycle_days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = cycle_days.len();
        let median = if n % 2 == 1 {
            cycle_days[n / 2]
        } else {
            (cycle_days[n / 2 - 1] + cycle_days[n / 2]) / 2.0
        };
        let rank = ((0.85 * n as f64).ceil() as usize).clamp(1, n);
        let round1 = |x: f64| (x * 10.0).round() / 10.0;
        json!({
            "median_days": round1(median),
            "p85_days": round1(cycle_days[rank - 1]),
            "n": n,
        })
    };

    let open: Vec<&Map<String, Value>> = by_id
        .values()
        .filter(|n| {
            has_pr(n)
                && str_field(n, "merge_status") != Some("merged")
                && n.get("merged_at").is_none()
                && str_field(n, "status") != Some("done")
                && str_field(n, "status") != Some("superseded")
        })
        .copied()
        .collect();

    let weeks: Vec<Value> = week_rows
        .iter()
        .map(|(label, _idx, partial)| {
            let (c, d) = buckets.get(label).copied().unwrap_or((0, 0));
            json!({"week_start": label, "code": c, "doc": d, "partial": partial})
        })
        .collect();
    let tz = local_dt(now_secs).map(|dt| dt.format("%:z").to_string());
    json!({
        "available": true,
        "window": {
            "since_days": since_days,
            "start": local_dt(window_start).map(|dt| dt.format("%Y-%m-%d").to_string()),
            "end": local_dt(now_secs).map(|dt| dt.format("%Y-%m-%d").to_string()),
            "week_start": "monday",
            "tz_offset": tz,
        },
        "deliveries": {
            "total": code + doc,
            "code": code,
            "doc": doc,
            "weeks": weeks,
        },
        "cycle": cycle,
        "open_prs": {
            "count": open.len(),
            "oldest_age_days": oldest_age_days(open.into_iter(), now_secs),
        },
        "waiting": {
            "in_progress": waiting_for("in_progress"),
            "in_review": waiting_for("in_review"),
            "blocked": waiting_for("blocked"),
        },
        "coverage": {
            "nodes": by_id.len(),
            "rows": rows.len(),
            "unlinked": unlinked_inferred + unlinked_rows,
            "age_basis": "node created_at",
        },
    })
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
    let vocab_terms: Vec<&str> = doc
        .iter()
        .chain(delivery.iter())
        .chain(ship.iter())
        .map(String::as_str)
        .collect();
    let flow = flow(
        &by_id,
        &rows,
        &by_node,
        params.get("now").and_then(Value::as_str),
        params
            .get("since_days")
            .and_then(Value::as_i64)
            .unwrap_or(28),
        &vocab_terms,
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
        "flow": flow,
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

    /// Wednesday-noon timestamps keep local-date bucketing stable on any
    /// test machine zone: no boundary shifts for offsets under 12h.
    #[test]
    fn flow_buckets_confirmed_deliveries_by_local_week() {
        let params = json!({
            "entries": [
                {"id": "x-1", "merge_status": "merged", "merged_at": "2026-09-02T12:00:00"},
                {"id": "x-2", "merge_status": "merged", "merged_at": "2026-09-08T12:00:00"},
                {"id": "d-1", "status": "done", "completed_at": "2026-09-03T12:00:00"},
                {"id": "v-1", "status": "done", "completed_at": "2026-09-04T12:00:00"}
            ],
            "rows": [
                {"graph_node_id": "d-1", "termination_reason": "DoneAdvisory", "completed": "2026-09-03T12:00:00"},
                {"graph_node_id": "v-1", "termination_reason": "DoneDelivery", "completed": "2026-09-04T12:00:00"}
            ],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
            "now": "2026-09-09T12:00:00",
            "since_days": 28
        });
        let out = classify(&params).unwrap();
        let flow = &out["flow"];
        assert_eq!(flow["available"], true);
        assert_eq!(flow["window"]["since_days"], 28);
        assert_eq!(flow["window"]["week_start"], "monday");
        // Aug 12 (window start, a Wednesday) .. Sep 9 (now): Mondays Aug 10
        // (partial), Aug 17, Aug 24, Aug 31, Sep 7 (current, partial).
        let weeks = flow["deliveries"]["weeks"].as_array().unwrap();
        assert_eq!(weeks.len(), 5);
        assert_eq!(weeks[0]["week_start"], "2026-08-10");
        assert_eq!(weeks[0]["partial"], true);
        assert_eq!(weeks[1]["partial"], false);
        assert_eq!(weeks[4]["week_start"], "2026-09-07");
        assert_eq!(weeks[4]["partial"], true);
        // Week of Aug 31: x-1 merged + v-1 explicit delivery (code), d-1 doc.
        // Week of Sep 7: x-2 merged.
        assert_eq!(weeks[3]["code"], 2);
        assert_eq!(weeks[3]["doc"], 1);
        assert_eq!(weeks[4]["code"], 1);
        assert_eq!(flow["deliveries"]["code"], 3);
        assert_eq!(flow["deliveries"]["doc"], 1);
    }

    #[test]
    fn flow_cycle_median_and_nearest_rank_p85() {
        // created_at is exactly n days before the merge, so elapsed days are
        // 1..=10: median 5.5, nearest-rank p85 the 9th sorted value.
        let mut entries = Vec::new();
        let merge = "2026-09-02T12:00:00";
        for n in 1..=10i64 {
            let created = (chrono::NaiveDateTime::parse_from_str(merge, "%Y-%m-%dT%H:%M:%S")
                .unwrap()
                - chrono::Duration::days(n))
            .format("%Y-%m-%dT%H:%M:%S");
            entries.push(json!({
                "id": format!("x-{n}"),
                "merge_status": "merged",
                "merged_at": merge,
                "created_at": created.to_string()
            }));
        }
        let params = json!({
            "entries": entries,
            "rows": [],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
            "now": "2026-09-09T12:00:00",
            "since_days": 28
        });
        let out = classify(&params).unwrap();
        let cycle = &out["flow"]["cycle"];
        assert_eq!(cycle["n"], 10);
        assert_eq!(cycle["median_days"], 5.5);
        // Nearest-rank p85 at n=10 is the 9th sorted value.
        assert_eq!(cycle["p85_days"], 9.0);
    }

    #[test]
    fn flow_unlinked_pr_row_rides_coverage_never_weeks() {
        let params = json!({
            "entries": [],
            "rows": [
                {"completed": "2026-09-02T12:00:00", "termination_reason": "DonePRGreen",
                 "pr_number": 404, "pr_url": "https://github.com/o/r/pull/404"},
                {"completed": "2026-09-02T12:00:00", "termination_reason": "DonePRGreen"}
            ],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
            "now": "2026-09-09T12:00:00",
            "since_days": 28
        });
        let out = classify(&params).unwrap();
        let flow = &out["flow"];
        assert_eq!(flow["deliveries"]["code"], 0);
        let weeks = flow["deliveries"]["weeks"].as_array().unwrap();
        assert!(weeks.iter().all(|w| w["code"] == 0 && w["doc"] == 0));
        assert_eq!(flow["coverage"]["unlinked"], 1);
    }

    #[test]
    fn flow_reports_waiting_and_open_prs_with_ages() {
        let params = json!({
            "entries": [
                {"id": "x-wip", "status": "in_progress", "created_at": "2026-08-30T12:00:00"},
                {"id": "x-blk", "status": "blocked", "created_at": "2026-09-06T12:00:00"},
                {"id": "x-open", "status": "in_review", "pr_number": 7,
                 "created_at": "2026-08-28T12:00:00"},
                {"id": "x-shipped", "status": "done", "pr_number": 8,
                 "merge_status": "merged", "merged_at": "2026-09-02T12:00:00",
                 "created_at": "2026-08-20T12:00:00"}
            ],
            "rows": [],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"],
            "now": "2026-09-09T12:00:00",
            "since_days": 28
        });
        let out = classify(&params).unwrap();
        let flow = &out["flow"];
        assert_eq!(flow["waiting"]["in_progress"]["count"], 1);
        assert_eq!(flow["waiting"]["in_progress"]["oldest_age_days"], 10);
        assert_eq!(flow["waiting"]["blocked"]["count"], 1);
        assert_eq!(flow["waiting"]["blocked"]["oldest_age_days"], 3);
        assert_eq!(flow["waiting"]["in_review"]["count"], 1);
        // x-shipped has a PR but is merged and done: never an open PR.
        assert_eq!(flow["open_prs"]["count"], 1);
        assert_eq!(flow["open_prs"]["oldest_age_days"], 12);
        assert_eq!(flow["coverage"]["age_basis"], "node created_at");
    }

    #[test]
    fn flow_is_unavailable_without_a_usable_now() {
        let params = json!({
            "entries": [node("x-1", Some("merged"))],
            "rows": [],
            "doc_terminals": ["DoneAdvisory"],
            "delivery_terminals": ["DoneDelivery"],
            "ship_terminals": ["DonePRGreen", "DoneBatched"]
        });
        let out = classify(&params).unwrap();
        assert_eq!(out["flow"]["available"], false);
        assert!(out["flow"]["reason"].as_str().is_some());
    }
}
