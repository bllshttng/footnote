//! The escalation-vs-stayed scoreboard view. `delegated` events carrying the
//! `handoff_kind=capability_escalation` marker join to per-node outcome
//! (merged, hours to merge, spend) and same-window comparison, per model,
//! against nodes that never escalated. The bucket model is the escalation
//! DESTINATION for an escalated node; the rest stay keyed on their first
//! attributed ledger model, so a model's escalated row reads "what
//! escalations TO this model do" next to "what plain runs ON this model do".

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::paths::AgentsHome;
use crate::scoreboard_provider::{num, parse_local, pct, percentile, round2};

/// The schema's positive escalation marker: a `delegated` event WITHOUT it
/// is legacy boundary delegation, never an escalation.
const CAPABILITY_ESCALATION: &str = "capability_escalation";
const UNKNOWN: &str = "unknown";

pub(crate) fn view(params: &Value) -> Result<Value, String> {
    let empty = Vec::new();
    let entries = params
        .get("entries")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let rows = params
        .get("rows")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let events = params
        .get("events")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let since_days = params
        .get("since_days")
        .and_then(Value::as_i64)
        .unwrap_or(28);
    let now = match params.get("now").and_then(Value::as_str) {
        Some(s) => parse_local(s).ok_or_else(|| format!("unparseable now {s:?}"))?,
        None => chrono::Local::now().naive_local(),
    };
    let built = build(rows, entries, events, since_days, now);
    let text = render(&built);
    Ok(json!({"view": built, "text": text}))
}

/// The escalation view's CLI body: the operator door under
/// `evals-macro --escalation`. Read-only; the sources and defaults mirror
/// `digest` (project journal first, then the global one, ledger and graph
/// store beside them).
pub fn run_escalation_view(since_days: i64, json_out: bool) -> i32 {
    if since_days < 1 {
        eprintln!("evals-macro: --since-days must be at least 1");
        return 2;
    }

    let home = AgentsHome::from_env();
    let fno_dir = home
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let mut events: Vec<Value> = Vec::new();
    for p in [
        PathBuf::from(".fno").join("events.jsonl"),
        fno_dir.join("events.jsonl"),
    ] {
        if let Ok(lines) = crate::loopcheck::event_lines(&p) {
            for line in lines {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    events.push(v);
                }
            }
        }
    }
    let ledger_raw = std::fs::read_to_string(fno_dir.join("ledger.json")).unwrap_or_default();
    let rows: Vec<Value> = serde_json::from_str::<Value>(&ledger_raw)
        .ok()
        .and_then(|v| {
            v.get("entries")
                .cloned()
                .or_else(|| Some(v))
                .and_then(|e| e.as_array().cloned())
        })
        .unwrap_or_default();
    let entries = match crate::graph_store::read_rows(&fno_dir.join("graph.db")) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("fno-agents: graph store unreadable: {e}");
            return 1;
        }
    };
    let reply = match view(&json!({
        "entries": entries,
        "rows": rows,
        "events": events,
        "since_days": since_days,
    })) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents: {e}");
            return 1;
        }
    };
    if json_out {
        println!("{}", reply["view"]);
    } else {
        print!("{}", reply["text"]);
    }
    0
}

/// One node's ledger-derived stats. `first_model` keys the stayed bucket:
/// the model of the node's EARLIEST attributed row, per the node's own
/// framing ("stayed on their first model").
struct NodeStat {
    first_model: Option<(chrono::NaiveDateTime, String)>,
    spend: f64,
}

impl Default for NodeStat {
    fn default() -> Self {
        NodeStat {
            first_model: None,
            spend: 0.0,
        }
    }
}

fn build(
    rows: &[Value],
    entries: &[Value],
    events: &[Value],
    since_days: i64,
    now: chrono::NaiveDateTime,
) -> Value {
    let cutoff = now - chrono::Duration::days(since_days);
    // Latest escalation per node: a later escalation's destination replaces
    // an earlier one's. An undated event loses to a dated one.
    let mut escalations: HashMap<&str, Option<chrono::NaiveDateTime>> = HashMap::new();
    let mut esc_models: HashMap<&str, String> = HashMap::new();
    for e in events {
        if event_kind(e) != Some("delegated") {
            continue;
        }
        let Some(nid) = str_field(e, "node_id") else {
            continue;
        };
        if str_field(e, "handoff_kind") != Some(CAPABILITY_ESCALATION) {
            continue;
        }
        let ts = e.get("ts").and_then(Value::as_str).and_then(parse_local);
        let replace = match escalations.get(nid) {
            None => true,
            Some(None) => ts.is_some(),
            Some(Some(old)) => ts.is_some_and(|t| t > *old),
        };
        if replace {
            escalations.insert(nid, ts);
            let model = str_field(e, "model").unwrap_or(UNKNOWN).to_string();
            esc_models.insert(nid, model);
        }
    }

    // The windowed ledger rows build the node stats; a node with rows but no
    // events joins as stayed-on-first-model.
    let mut nodes: BTreeMap<String, NodeStat> = BTreeMap::new();
    for r in rows {
        if r.get("type").and_then(Value::as_str) != Some("execution") {
            continue;
        }
        let Some(nid) = str_field(r, "graph_node_id") else {
            continue;
        };
        let Some(completed) = r
            .get("completed")
            .and_then(Value::as_str)
            .and_then(parse_local)
        else {
            continue;
        };
        if !(cutoff <= completed && completed <= now) {
            continue;
        }
        let st = nodes.entry(nid.to_string()).or_default();
        st.spend += num(r.get("cost_usd"));
        if let Some(model) = r
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let better = match &st.first_model {
                None => true,
                Some((t, _)) => completed < *t,
            };
            if better {
                st.first_model = Some((completed, model.to_string()));
            }
        }
    }

    // Escalation events inside the window admit a node even when its
    // post-escalation rows are not back yet: still open, merged false.
    for e in events {
        if event_kind(e) != Some("delegated") {
            continue;
        }
        let Some(nid) = str_field(e, "node_id") else {
            continue;
        };
        if str_field(e, "handoff_kind") != Some(CAPABILITY_ESCALATION) {
            continue;
        }
        let ts = e.get("ts").and_then(Value::as_str).and_then(parse_local);
        if ts.is_none_or(|t| cutoff <= t && t <= now) {
            nodes.entry(nid.to_string()).or_default();
        }
    }

    // Merged truth rides the node entry itself (`merge_status`): the view
    // answers "merged or not", not the delivery classifier's shipped-or-not.
    let mut by_id: HashMap<&str, &Map<String, Value>> = HashMap::new();
    for gn in entries {
        if let (Some(id), Some(obj)) = (gn.get("id").and_then(Value::as_str), gn.as_object()) {
            by_id.insert(id, obj);
        }
    }

    // Bucket per (model, escalated); each node counts once.
    let mut buckets: HashMap<(String, bool), Bucket> = HashMap::new();
    for (nid, st) in &nodes {
        let (escalated, model) = match escalations.get(nid.as_str()) {
            Some(_) => (
                true,
                esc_models
                    .get(nid.as_str())
                    .map(String::as_str)
                    .unwrap_or(UNKNOWN),
            ),
            None => (
                false,
                st.first_model
                    .as_ref()
                    .map(|(_, m)| m.as_str())
                    .unwrap_or(UNKNOWN),
            ),
        };
        let gn = by_id.get(nid.as_str()).copied();
        let merged = gn
            .map(|m| m.get("merge_status").and_then(Value::as_str) == Some("merged"))
            .unwrap_or(false);
        let hours = if merged {
            merge_instant(gn)
                .zip(created_at_of(gn))
                .map(|(end, start)| (end - start).num_minutes() as f64 / 60.0)
                .filter(|h| *h >= 0.0)
        } else {
            None
        };
        let b = buckets.entry((model.to_string(), escalated)).or_default();
        b.nodes += 1;
        if merged {
            b.merged += 1;
            if let Some(h) = hours {
                b.hours.push(h);
            }
        }
        b.spends.push(st.spend);
        b.spend += st.spend;
    }

    let mut out_rows: Vec<Value> = buckets
        .into_iter()
        .map(|((model, escalated), b)| {
            let total = b.nodes;
            json!({
                "model": model,
                "escalated": escalated,
                "nodes": b.nodes,
                "merged": b.merged,
                "open": total - b.merged,
                "merge_rate_pct": pct(b.merged, total),
                "median_hours_to_merge": percentile(&b.hours, 50),
                "median_spend_usd": percentile(&b.spends, 50),
                "spend_usd": round2(b.spend),
            })
        })
        .collect();
    out_rows.sort_by(|x, y| {
        let em = |v: &Value| v["model"].as_str().unwrap_or("").to_string();
        let esc = |v: &Value| v["escalated"].as_bool().unwrap_or(false);
        em(x).cmp(&em(y)).then_with(|| esc(y).cmp(&esc(x)))
    });

    let escalated_nodes = out_rows
        .iter()
        .filter(|r| r["escalated"].as_bool().unwrap_or(false))
        .map(|r| r["nodes"].as_u64().unwrap_or(0))
        .sum::<u64>();
    let known_model = out_rows
        .iter()
        .filter(|r| r["model"].as_str() != Some(UNKNOWN))
        .map(|r| r["nodes"].as_u64().unwrap_or(0))
        .sum::<u64>();
    let total_nodes: u64 = out_rows
        .iter()
        .map(|r| r["nodes"].as_u64().unwrap_or(0))
        .sum();
    if total_nodes == 0 {
        return json!({"state": "no_data", "since_days": since_days, "nodes": 0});
    }
    json!({
        "state": "ok",
        "since_days": since_days,
        "nodes": total_nodes,
        "escalated_nodes": escalated_nodes,
        "model_pct": pct(known_model, total_nodes),
        "rows": out_rows,
    })
}

/// The merge instant is `merged_at` first: it is the merge, not the
/// session's last activity. `completed_at` is the fallback for nodes that
/// predate the field.
fn merge_instant(node: Option<&Map<String, Value>>) -> Option<chrono::NaiveDateTime> {
    let n = node?;
    ["merged_at", "completed_at"]
        .iter()
        .find_map(|k| n.get(*k).and_then(Value::as_str).and_then(parse_local))
}

fn created_at_of(node: Option<&Map<String, Value>>) -> Option<chrono::NaiveDateTime> {
    let n = node?;
    n.get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_local)
}

/// Envelope-tolerant string field: the unified shape nests under `data`, the
/// legacy flat shape is top-level (digest.rs precedent).
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get("data")
        .and_then(|d| d.get(key))
        .or_else(|| v.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The event's `type` with a `kind` fallback for retired flat lines.
fn event_kind(v: &Value) -> Option<&str> {
    v.get("type")
        .and_then(Value::as_str)
        .or_else(|| v.get("kind").and_then(Value::as_str))
}

#[derive(Default)]
struct Bucket {
    nodes: u64,
    merged: u64,
    hours: Vec<f64>,
    spends: Vec<f64>,
    spend: f64,
}

fn render(built: &Value) -> String {
    let mut out = String::new();
    let win = built["since_days"].as_i64().unwrap_or(28);
    if built["state"] == "no_data" {
        out.push_str(&format!(
            "fno-agents scoreboard-escalation (last {win}d)\n\n  no node outcomes in window.\n"
        ));
        return out;
    }
    let nodes = built["nodes"].as_u64().unwrap_or(0);
    let esc_n = built["escalated_nodes"].as_u64().unwrap_or(0);
    out.push_str(&format!(
        "fno-agents scoreboard-escalation (last {win}d)\n\n  nodes in window: {nodes} (escalated {esc_n}, stayed {})\n  model attributed: {}% of nodes\n\n",
        nodes - esc_n,
        built["model_pct"]
    ));
    out.push_str(&format!(
        "  {:<26}{:<4}{:>6}{:>7}{:>7}{:>9}{:>12}{:>10}\n",
        "model", "esc", "nodes", "merged", "merge%", "med hrs", "med $/node", "spend$"
    ));
    for row in built["rows"].as_array().map_or(&[][..], |r| r.as_slice()) {
        let esc = if row["escalated"].as_bool().unwrap_or(false) {
            "yes"
        } else {
            ""
        };
        let hrs = row["median_hours_to_merge"]
            .as_f64()
            .map(|v| format!("{v}h"))
            .unwrap_or_else(|| "n/a".to_string());
        let med = row["median_spend_usd"]
            .as_f64()
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "n/a".to_string());
        out.push_str(&format!(
            "  {:<26}{:<4}{:>6}{:>7}{:>7}{:>9}{:>12}{:>10.2}\n",
            row["model"].as_str().unwrap_or_default(),
            esc,
            row["nodes"].as_i64().unwrap_or(0),
            row["merged"].as_i64().unwrap_or(0),
            row["merge_rate_pct"].as_i64().unwrap_or(0),
            hrs,
            med,
            row["spend_usd"].as_f64().unwrap_or(0.0),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc_event(nid: &str, model: &str, ts: &str, handoff: Option<&str>) -> Value {
        let mut e = json!({
            "ts": ts,
            "type": "delegated",
            "data": {"node_id": nid, "model": model},
        });
        if let Some(h) = handoff {
            e["data"]["handoff_kind"] = json!(h);
        }
        e
    }

    fn row(nid: &str, model: Option<&str>, tr: &str, cost: f64, completed: &str) -> Value {
        let mut r = json!({
            "type": "execution",
            "graph_node_id": nid,
            "termination_reason": tr,
            "cost_usd": cost,
            "completed": completed,
        });
        if let Some(m) = model {
            r["model"] = json!(m);
        }
        r
    }

    fn call(rows: &[Value], entries: &[Value], events: &[Value]) -> Value {
        view(&json!({
            "entries": entries,
            "rows": rows,
            "events": events,
            "since_days": 5,
            "now": "2026-07-03T20:00:00Z",
        }))
        .expect("view runs")
    }

    #[test]
    fn escalated_and_stayed_nodes_compare_per_model() {
        let entries = vec![
            json!({"id": "x-1", "merge_status": "merged", "created_at": "2026-07-01T00:00:00", "merged_at": "2026-07-02T12:00:00"}),
            json!({"id": "x-2", "merge_status": "merged", "created_at": "2026-06-30T18:00:00", "completed_at": "2026-07-02T06:00:00"}),
            json!({"id": "x-3", "merge_status": null}),
        ];
        let rows = vec![
            row(
                "x-1",
                Some("glm-5.3-flash"),
                "NoProgress",
                1.0,
                "2026-07-01T10:00:00",
            ),
            row(
                "x-1",
                Some("gpt-6-luna"),
                "DonePRGreen",
                2.0,
                "2026-07-02T10:00:00",
            ),
            row(
                "x-2",
                Some("glm-5.3-flash"),
                "DonePRGreen",
                1.0,
                "2026-07-02T05:00:00",
            ),
            row(
                "x-2",
                Some("opus"),
                "NoProgress",
                0.5,
                "2026-07-02T11:00:00",
            ),
            row(
                "x-3",
                Some("glm-5.3-flash"),
                "NoProgress",
                1.0,
                "2026-07-02T10:00:00",
            ),
        ];
        let events = vec![
            esc_event(
                "x-1",
                "gpt-6-luna",
                "2026-07-01T09:00:00Z",
                Some("capability_escalation"),
            ),
            esc_event("x-3", "gpt-6-luna", "2026-07-01T09:00:00Z", None),
        ];
        let v = call(&rows, &entries, &events);
        let view = &v["view"];
        assert_eq!(view["state"], "ok", "{v}");
        assert_eq!(view["escalated_nodes"], 1, "{v}");
        let rs = view["rows"].as_array().unwrap();
        let esc = rs.iter().find(|r| r["escalated"] == true).unwrap();
        assert_eq!(esc["model"], "gpt-6-luna");
        assert_eq!(esc["nodes"], 1);
        assert_eq!(esc["merged"], 1, "{esc}");
        assert_eq!(esc["median_hours_to_merge"], 36.0);
        assert_eq!(esc["spend_usd"], 3.0, "node spend sums ALL its rows: {esc}");
        let stayed = rs
            .iter()
            .find(|r| r["model"] == "glm-5.3-flash" && r["escalated"] == false)
            .unwrap();
        assert_eq!(stayed["nodes"], 2);
        assert_eq!(stayed["merged"], 1);
        assert_eq!(stayed["median_hours_to_merge"], 36.0);
        assert_eq!(stayed["spend_usd"], 2.5);
        assert_eq!(
            view["model_pct"], 100,
            "every node carries a known model: {view}"
        );
    }

    #[test]
    fn junk_events_and_empty_window_degrade_cleanly() {
        // Empty window: a named no_data state, not an empty table.
        let empty = call(&[], &[], &[]);
        assert_eq!(empty["view"]["state"], "no_data", "{empty}");
        assert!(empty["text"]
            .as_str()
            .unwrap()
            .contains("no node outcomes in window"));
        // Junk in, structured out: an event with no node_id is skipped
        // entirely; the rowless-model node reads the unknown stayed bucket,
        // unmerged with null hours and junk cost at 0.0.
        let events = vec![json!({
            "ts": "2026-07-03T10:00:00Z", "type": "delegated",
            "data": {"handoff_kind": "capability_escalation"},
        })];
        let rows = vec![row("x-1", None, "DonePRGreen", 1.0, "2026-07-03T10:00:00")];
        let v = call(&rows, &[json!({"id": "x-1"})], &events);
        let view = &v["view"];
        assert_eq!(view["state"], "ok", "{v}");
        assert_eq!(view["escalated_nodes"], 0, "{v}");
        let r = &view["rows"].as_array().unwrap()[0];
        assert_eq!(r["model"], "unknown");
        assert_eq!(r["escalated"], false);
        assert_eq!(r["merged"], 0);
        assert_eq!(r["median_hours_to_merge"], Value::Null);
        assert_eq!(r["spend_usd"], 1.0);
    }
}
