//! The by-provider fold and renderer, ported from
//! `cli/src/fno/scoreboard/fold.py build_provider_scoreboard` and
//! `cli.py _render_by_provider` (x-59e1). The group key is now
//! `(harness, provider, model)`; `provider_id` is not read. Deliveries come
//! from the one classifier (`crate::scoreboard::classify`) in process, the
//! same answer `classify_deliveries` gets in Python.

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

const UNATTRIBUTED: &str = "unattributed";
const UNRECORDED: &str = "unrecorded";
const UNKNOWN: &str = "unknown";
/// `_SURVIVAL_FOLLOWUP_DAYS` (fold.py:123): a caused_by fix inside the
/// window bounces the node; an un-time-boundable fix counts against it.
const FOLLOWUP_DAYS: i64 = 14;
/// The same terminal lists `request_scoreboard_classify` sends
/// (cli/src/fno/graph/store.py): Python's vocabulary stays authoritative.
const DOC_TERMINALS: [&str; 1] = ["DoneAdvisory"];
const DELIVERY_TERMINALS: [&str; 1] = ["DoneDelivery"];
/// sorted(DELIVERED_TERMINALS - {DoneAdvisory, DoneDelivery}), fno/terminals.py.
const SHIP_TERMINALS: [&str; 2] = ["DoneBatched", "DonePRGreen"];

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
    let since_days = params
        .get("since_days")
        .and_then(Value::as_i64)
        .unwrap_or(28);
    let now = match params.get("now").and_then(Value::as_str) {
        Some(s) => parse_local(s).ok_or_else(|| format!("unparseable now {s:?}"))?,
        None => chrono::Local::now().naive_local(),
    };
    let built = build(rows, entries, since_days, now)?;
    let text = render(&built);
    Ok(json!({"view": built, "text": text}))
}

#[derive(Default)]
struct Bucket {
    runs: u64,
    shipped: u64,
    shipped_linked: u64,
    bounced: u64,
    spend: f64,
    measured_cost: bool,
    iterations: Vec<f64>,
    nids: HashSet<String>,
    delivered_nids: HashSet<String>,
    nid_rows: u64,
}

fn build(
    rows: &[Value],
    entries: &[Value],
    since_days: i64,
    now: chrono::NaiveDateTime,
) -> Result<Value, String> {
    let cutoff = now - chrono::Duration::days(since_days);
    let windowed: Vec<&Value> = rows
        .iter()
        .filter(|r| {
            r.get("type").and_then(Value::as_str) == Some("execution")
                && r.get("completed")
                    .and_then(Value::as_str)
                    .and_then(parse_local)
                    .is_some_and(|dt| cutoff <= dt && dt <= now)
        })
        .collect();
    let total = windowed.len();
    if total == 0 {
        return Ok(json!({"state": "no_data", "since_days": since_days, "rows": 0}));
    }
    let cls = crate::scoreboard::classify(&json!({
        "entries": entries,
        "rows": rows,
        "doc_terminals": DOC_TERMINALS,
        "delivery_terminals": DELIVERY_TERMINALS,
        "ship_terminals": SHIP_TERMINALS,
    }))?;
    let empty_map = Map::new();
    let by_node = cls
        .get("by_node")
        .and_then(Value::as_object)
        .unwrap_or(&empty_map);

    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    let mut fixes: HashMap<&str, Vec<&Value>> = HashMap::new();
    let w4_available = entries.iter().any(|n| {
        n.get("reverted").is_some()
            || n.get("caused_by")
                .map_or(false, |v| !v.is_null() && *v != json!(false))
    });
    for gn in entries {
        if let Some(id) = gn.get("id").and_then(Value::as_str) {
            by_id.insert(id, gn);
            if let Some(origin) = gn.get("caused_by").and_then(Value::as_str) {
                if !origin.is_empty() {
                    fixes.entry(origin).or_default().push(gn);
                }
            }
        }
    }

    let key = |r: &Value, k: &str, fallback: &'static str| -> String {
        r.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback)
            .to_string()
    };

    let mut buckets: HashMap<(String, String, String), Bucket> = HashMap::new();
    let mut order: Vec<(String, String, String)> = Vec::new();
    let mut node_buckets: HashMap<String, HashSet<(String, String, String)>> = HashMap::new();
    let mut harness_n = 0usize;
    let mut provider_n = 0usize;
    let mut model_n = 0usize;
    let mut attributed_n = 0usize;
    for r in &windowed {
        let harness = key(r, "harness", UNATTRIBUTED);
        let provider = key(r, "provider", UNRECORDED);
        let model = key(r, "model", UNKNOWN);
        if r.get("harness")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            harness_n += 1;
        }
        if r.get("provider")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            provider_n += 1;
        }
        if r.get("model")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            model_n += 1;
        }
        if r.get("harness")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
            && r.get("model")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        {
            attributed_n += 1;
        }
        let k = (harness.clone(), provider.clone(), model.clone());
        if !buckets.contains_key(&k) {
            order.push(k.clone());
        }
        let b = buckets.entry(k.clone()).or_default();
        b.runs += 1;
        let shipped_now = row_shipped(r, by_node);
        b.shipped += u64::from(shipped_now);
        b.spend += num(r.get("cost_usd"));
        if num_opt(r.get("cost_usd")).is_some() {
            b.measured_cost = true;
        }
        if let Some(nid) = r
            .get("graph_node_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            b.nids.insert(nid.to_string());
            b.nid_rows += 1;
        }
        if shipped_now {
            if let Some(it) = num_opt(r.get("iterations")) {
                b.iterations.push(it);
            }
            if let Some(nid) = r
                .get("graph_node_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                // Delivered nodes count ONCE per bucket; retries never stack credit.
                b.delivered_nids.insert(nid.to_string());
                node_buckets
                    .entry(nid.to_string())
                    .or_default()
                    .insert(k.clone());
            }
            if w4_available
                && r.get("graph_node_id")
                    .and_then(Value::as_str)
                    .is_some_and(|nid| by_id.contains_key(nid))
            {
                b.shipped_linked += 1;
                let outcome = node_outcome(
                    r.get("graph_node_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    r.get("completed")
                        .and_then(Value::as_str)
                        .and_then(parse_local),
                    &by_id,
                    &fixes,
                );
                if outcome == "bounced" || outcome == "reverted" {
                    b.bounced += 1;
                }
            }
        }
    }

    let shared: HashSet<String> = node_buckets
        .iter()
        .filter(|(_, ks)| ks.len() > 1)
        .map(|(nid, _)| nid.clone())
        .collect();
    let mut out_rows: Vec<Value> = Vec::new();
    for k in &order {
        let b = &buckets[k];
        let cps = if b.shipped > 0 && b.measured_cost {
            Some(round2(b.spend / b.shipped as f64))
        } else {
            None
        };
        let bounce = if b.shipped_linked > 0 {
            Some(pct(b.bounced, b.shipped_linked))
        } else {
            None
        };
        out_rows.push(json!({
            "harness": k.0,
            "provider": k.1,
            "model": k.2,
            "runs": b.runs,
            "shipped": b.shipped,
            "delivered_nodes": b.delivered_nids.len(),
            "shared_nodes": b.delivered_nids.iter().filter(|n| shared.contains(*n)).count(),
            "spend_usd": round2(b.spend),
            "cost_per_shipped_usd": cps,
            "bounce_rate_pct": bounce,
            "shipped_linked": b.shipped_linked,
            "median_iterations": percentile(&b.iterations, 50),
            "retry_rows": b.nid_rows.saturating_sub(b.nids.len() as u64),
        }));
    }
    // Unattributed buckets sorted last, then by harness, then spend desc.
    fn spend_of(v: &Value) -> f64 {
        v["spend_usd"].as_f64().unwrap_or(0.0)
    }
    fn axis_of<'a>(v: &'a Value, k: &str) -> &'a str {
        v[k].as_str().unwrap_or_default()
    }
    out_rows.sort_by(|x, y| {
        (axis_of(x, "harness") == UNATTRIBUTED)
            .cmp(&(axis_of(y, "harness") == UNATTRIBUTED))
            .then_with(|| axis_of(x, "harness").cmp(axis_of(y, "harness")))
            .then_with(|| spend_of(y).total_cmp(&spend_of(x)))
            .then_with(|| axis_of(x, "model").cmp(axis_of(y, "model")))
            .then_with(|| axis_of(x, "provider").cmp(axis_of(y, "provider")))
    });

    Ok(json!({
        "state": "ok",
        "since_days": since_days,
        "coverage": {
            "rows": total,
            "harness_pct": pct(harness_n as u64, total as u64),
            "provider_pct": pct(provider_n as u64, total as u64),
            "model_pct": pct(model_n as u64, total as u64),
            "attributed_pct": pct(attributed_n as u64, total as u64),
        },
        "rows": out_rows,
    }))
}

/// The ported `_row_shipped` (fold.py:91): a terminal proves the ROW shipped
/// only when its node delivered; a reconcile backstop is a delivery row even
/// though it is not a worker terminal.
fn row_shipped(row: &Value, by_node: &Map<String, Value>) -> bool {
    let tr = row
        .get("termination_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_backstop =
        tr == "reconcile-backstop" && row.get("pr_number").map_or(false, |v| !v.is_null());
    if !SHIP_TERMINALS.contains(&tr) && !is_backstop {
        return false;
    }
    match row
        .get("graph_node_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        None => true,
        Some(nid) => by_node
            .get(nid)
            .and_then(|c| c.get("delivered"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// The ported `_node_outcome` (fold.py:488): `reverted` flag, else a
/// caused_by fix node within the follow-up window -> bounced, else
/// merged_clean. An un-time-boundable fix counts against the node.
fn node_outcome(
    nid: &str,
    shipped_at: Option<chrono::NaiveDateTime>,
    by_id: &HashMap<&str, &Value>,
    fixes: &HashMap<&str, Vec<&Value>>,
) -> &'static str {
    if by_id
        .get(nid)
        .and_then(|n| n.get("reverted"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "reverted";
    }
    if let Some(list) = fixes.get(nid) {
        for fx in list {
            let fx_at = fx
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_local);
            match (shipped_at, fx_at) {
                (Some(s), Some(f)) => {
                    let delta = f - s;
                    if delta >= chrono::Duration::zero()
                        && delta <= chrono::Duration::days(FOLLOWUP_DAYS)
                    {
                        return "bounced";
                    }
                }
                _ => return "bounced",
            }
        }
    }
    "merged_clean"
}

/// The fold's `_num`: junk-tolerant cost coercion, junk -> 0.0. Unlike
/// `_num_opt`, a finite NEGATIVE reads as itself (Python's `float(v or 0.0)`
/// never filtered the sign); the spend column stays sign-faithful.
fn num(v: Option<&Value>) -> f64 {
    let Some(v) = v else { return 0.0 };
    let f = match v {
        Value::Number(_) => v.as_f64().unwrap_or(f64::NAN),
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::String(s) if !s.is_empty() => s.parse::<f64>().unwrap_or(f64::NAN),
        _ => return 0.0,
    };
    if f.is_finite() {
        f
    } else {
        0.0
    }
}

/// The fold's `_num_opt`: a MISSING/None/junk value is None, not 0.0, and a
/// non-finite or negative reading stays None.
fn num_opt(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    let f = match v {
        Value::Number(_) => v.as_f64()?,
        Value::Bool(_) => return None,
        Value::String(s) if !s.is_empty() => s.parse::<f64>().ok()?,
        _ => return None,
    };
    if f.is_finite() && f >= 0.0 {
        Some(f)
    } else {
        None
    }
}

/// The ported `_pct` (fold.py:325): `round()` in Python is half-to-even, so
/// an integer build keeps a tie from flipping a digit the old view printed.
fn pct(n: u64, d: u64) -> i64 {
    if d == 0 {
        return 0;
    }
    let num = 100u128 * n as u128;
    let q = num / d as u128;
    let twice = (num % d as u128) * 2;
    let out = if twice > d as u128 || (twice == d as u128 && q % 2 == 1) {
        q + 1
    } else {
        q
    };
    out as i64
}

fn round2(f: f64) -> f64 {
    (f * 100.0).round() / 100.0
}

/// The ported `_percentile`: nearest-rank over a sorted copy; None on empty.
fn percentile(values: &[f64], p: u64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut s = values.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let k = (((p as f64) / 100.0) * s.len() as f64).ceil() as usize;
    s.get(k.saturating_sub(1).min(s.len() - 1)).copied()
}

/// The fold's `_parse_ts` shape: aware stamps land on the local timeline
/// before their tzinfo is stripped, so an offset is not a shift.
fn parse_local(raw: &str) -> Option<chrono::NaiveDateTime> {
    let normalized = raw.replace('Z', "+00:00");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.with_timezone(&chrono::Local).naive_local());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt);
    }
    if let Ok(dt) = chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        return dt.and_hms_opt(0, 0, 0);
    }
    None
}

/// The ported `_render_by_provider` (cli.py:434): the harness column leads
/// and blanks on a repeat like the old provider column did; bounce rides
/// with its denominator; the coverage block reports all four percentages.
fn render(pb: &Value) -> String {
    let mut out = String::new();
    let win = pb["since_days"].as_i64().unwrap_or(28);
    if pb["state"] == "no_data" {
        out.push_str(&format!("fno whoami scoreboard --by-provider (last {win}d)\n\n  no terminal sessions in window.\n"));
        return out;
    }
    let cov = &pb["coverage"];
    out.push_str(&format!(
        "fno whoami scoreboard --by-provider (last {win}d)\n\nCoverage\n  rows in window:      {} execution rows\n",
        cov["rows"]
    ));
    for (label, key) in [
        ("harness", "harness_pct"),
        ("provider", "provider_pct"),
        ("model", "model_pct"),
    ] {
        out.push_str(&format!("  {label:<19}{}%\n", cov[key]));
    }
    out.push_str(&format!(
        "  attributed:          {}%\n",
        cov["attributed_pct"]
    ));
    if cov["attributed_pct"].as_i64().unwrap_or(0) < 100 {
        out.push_str(&format!(
            "  ! rows below reflect {}% harness attribution - unattributed rows are a visible bucket, never dropped.\n",
            cov["attributed_pct"]
        ));
    }
    out.push_str(&format!(
        "\n  {:<12}{:<16}{:<22}{:>6}{:>7}{:>7}{:>8}{:>10}{:>9}{:>13}{:>10}{:>9}\n",
        "harness",
        "provider",
        "model",
        "runs",
        "ships",
        "nodes",
        "shared",
        "spend$",
        "$/ship",
        "bounce%",
        "med iter",
        "retries"
    ));
    let mut prev = String::new();
    for row in pb["rows"].as_array().map_or(&[][..], |r| r.as_slice()) {
        let harness = row["harness"].as_str().unwrap_or_default();
        let shown = if harness == prev { "" } else { harness };
        prev = harness.to_string();
        let cps = row["cost_per_shipped_usd"]
            .as_f64()
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "n/a".to_string());
        let bounce = row["bounce_rate_pct"]
            .as_i64()
            .map(|v| format!("{v}% of {}", row["shipped_linked"]))
            .unwrap_or_else(|| "n/a".to_string());
        let med = row["median_iterations"]
            .as_f64()
            .map(|v| {
                if v.fract() == 0.0 {
                    format!("{}", v as i64)
                } else {
                    format!("{v}")
                }
            })
            .unwrap_or_else(|| "n/a".to_string());
        out.push_str(&format!(
            "  {shown:<12}{:<16}{:<22}{:>6}{:>7}{:>7}{:>8}{:>10.2}{:>9}{bounce:>13}{med:>10}{:>9}\n",
            row["provider"].as_str().unwrap_or_default(),
            row["model"].as_str().unwrap_or_default(),
            row["runs"].as_i64().unwrap_or(0),
            row["shipped"].as_i64().unwrap_or(0),
            row["delivered_nodes"].as_i64().unwrap_or(0),
            row["shared_nodes"].as_i64().unwrap_or(0),
            row["spend_usd"].as_f64().unwrap_or(0.0),
            cps,
            row["retry_rows"].as_i64().unwrap_or(0),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis_row(h: Option<&str>, m: Option<&str>, tr: &str, nid: Option<&str>) -> Value {
        let mut r = json!({
            "type": "execution",
            "completed": "2026-07-03T10:00:00",
            "termination_reason": tr,
            "cost_usd": 1.0,
        });
        if let Some(v) = h {
            r["harness"] = json!(v);
        }
        if let Some(v) = m {
            r["model"] = json!(v);
        }
        if let Some(v) = nid {
            r["graph_node_id"] = json!(v);
        }
        r
    }

    fn call(rows: &[Value], entries: &[Value]) -> Value {
        view(&json!({
            "entries": entries,
            "rows": rows,
            "since_days": 5,
            "now": "2026-07-03T20:00:00Z",
        }))
        .expect("view runs")
    }

    fn graph() -> Vec<Value> {
        vec![
            json!({"id": "x-1", "reverted": false, "merge_status": "merged", "completed_at": "2026-07-03T10:00:00"}),
            json!({"id": "x-2", "reverted": false, "merge_status": "merged", "completed_at": "2026-07-03T10:00:00"}),
        ]
    }

    #[test]
    fn grouping_and_cost_per_ship() {
        let rows = vec![
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1")),
            axis_row(Some("claude"), Some("opus"), "NoProgress", None),
            axis_row(Some("codex"), Some("gpt"), "DonePRGreen", Some("x-2")),
        ];
        let v = call(&rows, &graph());
        assert_eq!(v["view"]["state"], "ok", "{v}");
        let rs = v["view"]["rows"].as_array().unwrap();
        assert_eq!(rs.len(), 2, "two buckets: {v}");
        let a = &rs[0];
        assert_eq!(a["harness"], "claude");
        assert_eq!(a["provider"], "unrecorded");
        assert_eq!(a["model"], "opus");
        assert_eq!(a["runs"], 2);
        assert_eq!(a["shipped"], 1);
        assert_eq!(a["delivered_nodes"], 1);
        assert_eq!(a["spend_usd"], 2.0);
        assert_eq!(a["cost_per_shipped_usd"], 2.0);
        let b = &rs[1];
        assert_eq!(b["harness"], "codex");
        assert_eq!(b["model"], "gpt");
        // The ported _pct rounds half-to-even, like Python's round().
        assert_eq!(pct(1, 8), 12, "12.5 never rounds up");
        assert_eq!(pct(3, 8), 38, "37.5 rounds to the even 38");
    }

    #[test]
    fn non_execution_rows_and_window_cut_excluded() {
        let mut rows = vec![
            json!({"type": "cost", "completed": "2026-07-03T10:00:00", "cost_usd": 9.0}),
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1")),
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", None),
        ];
        rows[2]["completed"] = json!("2026-06-01T10:00:00");
        rows.push(
            json!({"type": "execution", "completed": "2026-07-10T10:00:00", "harness": "claude"}),
        );
        let v = call(&rows, &graph());
        assert_eq!(v["view"]["coverage"]["rows"], 2, "{v}");
        assert_eq!(v["view"]["rows"][0]["runs"], 2);
    }

    #[test]
    fn delivered_nodes_once_with_shared_credit() {
        let rows = vec![
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1")),
            axis_row(Some("codex"), Some("gpt"), "DonePRGreen", Some("x-1")),
        ];
        let v = call(&rows, &graph());
        for r in v["view"]["rows"].as_array().unwrap() {
            assert_eq!(r["delivered_nodes"], 1, "{r}");
            assert_eq!(r["shared_nodes"], 1);
        }
    }

    #[test]
    fn retry_rows_count_repeat_nids() {
        let rows = vec![
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1")),
            axis_row(Some("claude"), Some("opus"), "NoProgress", Some("x-1")),
        ];
        let v = call(&rows, &graph());
        let r = &v["view"]["rows"][0];
        assert_eq!(r["delivered_nodes"], 1, "{r}");
        assert_eq!(r["retry_rows"], 1);
    }

    #[test]
    fn bounce_needs_causal_telemetry_and_median_rides() {
        let mut entries = graph();
        entries.push(json!({
            "id": "x-fix", "caused_by": "x-1", "created_at": "2026-07-04T00:00:00"
        }));
        let mut r1 = axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1"));
        r1["iterations"] = json!(3);
        let mut r2 = axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1"));
        r2["iterations"] = json!(7);
        let v = call(&[r1, r2], &entries);
        let r = &v["view"]["rows"][0];
        assert_eq!(r["bounce_rate_pct"], 100, "{r}");
        assert_eq!(r["shipped_linked"], 2);
        assert_eq!(r["median_iterations"], 5.0);
        let v = call(
            &[axis_row(
                Some("claude"),
                Some("opus"),
                "DonePRGreen",
                Some("x-1"),
            )],
            &[json!({"id": "x-1"})],
        );
        let r = &v["view"]["rows"][0];
        assert_eq!(
            r["bounce_rate_pct"],
            Value::Null,
            "no W4 fields, no fake 0%: {r}"
        );
        assert_eq!(r["shipped_linked"], 0);
    }

    #[test]
    fn unattributed_never_dropped_and_spend_reconciles() {
        let rows = vec![
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1")),
            axis_row(None, None, "NoProgress", None),
            axis_row(Some("codex"), None, "DonePRGreen", Some("x-2")),
        ];
        let v = call(&rows, &graph());
        let rs = v["view"]["rows"].as_array().unwrap();
        let total: f64 = rs
            .iter()
            .map(|r| r["spend_usd"].as_f64().unwrap_or(0.0))
            .sum();
        assert!((total - 3.0).abs() < 1e-9, "spend reconciles: {v}");
        assert_eq!(v["view"]["coverage"]["harness_pct"], 67);
        assert_eq!(v["view"]["coverage"]["model_pct"], 67);
        assert_eq!(v["view"]["coverage"]["attributed_pct"], 67, "{v}");
        let last = rs.last().unwrap();
        assert_eq!(
            last["harness"], "unattributed",
            "unattributed sorted last: {v}"
        );
    }

    #[test]
    fn junk_values_never_crash_the_fold() {
        let mut rows = vec![axis_row(
            Some("claude"),
            Some("opus"),
            "DonePRGreen",
            Some("x-1"),
        )];
        rows[0]["cost_usd"] = json!("abc");
        rows[0]["iterations"] = json!("junk");
        rows[0]["model"] = json!(123);
        let v = call(&rows, &graph());
        let r = &v["view"]["rows"][0];
        assert_eq!(r["spend_usd"], 0.0, "{r}");
        assert_eq!(r["cost_per_shipped_usd"], Value::Null, "no measured cost");
        assert_eq!(r["median_iterations"], Value::Null);
        assert_eq!(
            r["model"], "unknown",
            "non-string lands in the fallback bucket"
        );
    }

    #[test]
    fn rendered_text_blank_repeats_and_coverage_lines() {
        let rows = vec![
            axis_row(Some("claude"), Some("opus"), "DonePRGreen", Some("x-1")),
            axis_row(Some("claude"), Some("gpt"), "DonePRGreen", Some("x-2")),
            axis_row(None, None, "NoProgress", None),
        ];
        let v = call(&rows, &graph());
        let text = v["text"].as_str().unwrap();
        assert!(text.contains("Coverage"), "{text}");
        assert!(
            text.contains("harness:") && text.contains("provider:") && text.contains("model:"),
            "{text}"
        );
        assert!(text.contains("attributed:"), "{text}");
        assert!(
            text.contains("! rows below reflect"),
            "below 100 warns: {text}"
        );
        let body = text.split("med iter").nth(1).unwrap_or_default();
        let blanked = body.lines().filter(|l| l.starts_with("              "));
        assert!(blanked.count() >= 1, "a repeated harness blanks: {text}");
        let empty = call(&[], &graph());
        assert!(empty["text"]
            .as_str()
            .unwrap()
            .contains("no terminal sessions in window"));
    }
}
