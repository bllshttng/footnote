//! `route-slot`: the delivery-slot resolver, ported from Python
//! (law d-450caaeb: Rust is the product; Python is the compatibility shell).
//!
//! The verb answers ONE question: which configured lane does this dispatch
//! ride right now? Input is a JSON payload (profile fields, raw lanes,
//! declared routing rows, the capacity snapshot, posture flags, vendor
//! counts/caps); output is JSON `{status, candidate, chain}` where `chain`
//! carries the receipt vocabulary VERBATIM - the Python spawn seam, advance
//! and the doctor match on these strings, so rewording here would fork the
//! receipt language. Selection logic only: the capacity snapshot and vendor
//! live counts are gathered by the caller (Python input adapters over the
//! attribution owners); this module never reads state files or the network.

use serde_json::{json, Map, Value};
use std::path::Path;

const SLOT_LANE_FIELDS: [&str; 9] = [
    "provider",
    "model",
    "effort",
    "substrate",
    "permission_mode",
    "route",
    "account",
    "pane_group",
    "args",
];
const LANE_PASSTHROUGH_FIELDS: [&str; 4] = ["substrate", "permission_mode", "pane_group", "args"];
const ON_EXHAUSTED: [&str; 3] = ["queue", "degrade", "refuse"];
const ON_LOW: [&str; 3] = ["allow", "prefer_healthy", "skip"];
const ON_UNKNOWN: [&str; 2] = ["allow", "skip"];
const DIFFICULTY_KEYS: [&str; 3] = ["low", "medium", "high"];

/// The effective overlay key for a dispatch; missing or odd rounds up to high
/// with the reason (`xhigh` is an effort value, never a node band).
fn effective_difficulty(node_difficulty: Option<&str>) -> (String, Option<String>) {
    match node_difficulty.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match raw.to_lowercase().as_str() {
            k @ ("low" | "medium" | "high") => (k.to_string(), None),
            other => (
                "high".to_string(),
                Some(format!(
                    "difficulty '{other}' is not low|medium|high; rounds up to high"
                )),
            ),
        },
        None => (
            "high".to_string(),
            Some("difficulty missing; rounds up to high".to_string()),
        ),
    }
}

fn row_value<'a>(row: &'a Value, key: &str) -> String {
    row.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// One walk target: the config-path rung plus the row name it resolves to.
fn plan_entry(rung_base: &str, index: usize, raw: &Value) -> Option<(String, String)> {
    let rung = format!("{rung_base}.lanes[{index}]");
    match raw {
        Value::String(name) => {
            let name = name.trim();
            if name.is_empty() {
                None
            } else {
                Some((rung, name.to_string()))
            }
        }
        Value::Object(_) => Some((rung.clone(), rung)),
        _ => None,
    }
}

/// Fold the raw lane list to `(plan, rows, fields_by_rung)` against the
/// declared rows; a fault returns its config-terminal line.
#[allow(clippy::type_complexity)]
fn fold(
    rung_base: &str,
    lanes_raw: &[Value],
    declared_rows: &Map<String, Value>,
) -> Result<
    (
        Vec<(String, String)>,
        Map<String, Value>,
        Map<String, Value>,
    ),
    String,
> {
    let mut plan = Vec::new();
    let mut rows = Map::new();
    let mut fields_by_rung: Map<String, Value> = Map::new();
    let fault = |rung: &str, why: String| format!("slot=config {rung} {why}");

    for (index, raw) in lanes_raw.iter().enumerate() {
        let rung = format!("{rung_base}.lanes[{index}]");
        match raw {
            Value::String(name) => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(fault(&rung, "is an empty lane name".into()));
                }
                plan.push((rung, name.to_string()));
            }
            Value::Object(table) => {
                let unknown: Vec<&String> = table
                    .keys()
                    .filter(|k| !SLOT_LANE_FIELDS.contains(&k.as_str()))
                    .collect();
                if let Some(first) = unknown.first() {
                    return Err(fault(&rung, format!("has unknown field '{first}'")));
                }
                for (k, v) in table {
                    if k == "args" {
                        // x-8975: the lane's native-bundle vector, opaque and
                        // passed through verbatim; never a ranked field.
                        match v.as_array() {
                            Some(items)
                                if !items.is_empty() && items.iter().all(Value::is_string) => {}
                            _ => {
                                return Err(fault(
                                    &rung,
                                    format!(".args must be a non-empty list of strings; got {v}"),
                                ))
                            }
                        }
                        continue;
                    }
                    if !v.is_string() {
                        return Err(fault(&rung, format!(".{k} must be a string; got {v}")));
                    }
                }
                let get = |k: &str| {
                    table
                        .get(k)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string()
                };
                let mut fields = Map::new();
                for k in SLOT_LANE_FIELDS {
                    let v = get(k);
                    if !v.is_empty() {
                        fields.insert(k.to_string(), Value::String(v));
                    }
                }
                // args is an array, not a string, so the loop above cannot
                // carry it; copy it verbatim (x-8975).
                if let Some(args) = table.get("args").and_then(Value::as_array) {
                    fields.insert("args".into(), Value::Array(args.clone()));
                }
                if fields.is_empty() {
                    return Err(fault(&rung, "is empty".into()));
                }
                let mut row = Map::new();
                row.insert("name".into(), Value::String(rung.clone()));
                for k in SLOT_LANE_FIELDS {
                    let v = get(k);
                    if !v.is_empty() && !LANE_PASSTHROUGH_FIELDS.contains(&k) {
                        let key = if k == "provider" { "harness" } else { k };
                        row.insert(key.to_string(), Value::String(v));
                    }
                }
                rows.insert(rung.clone(), Value::Object(row));
                fields_by_rung.insert(rung.clone(), Value::Object(fields));
                plan.push((rung.clone(), rung));
            }
            _ => {
                return Err(fault(
                    &rung,
                    "must be a table or a [[routing.models]] row name".into(),
                ));
            }
        }
    }
    for (name, row) in declared_rows {
        rows.insert(name.clone(), row.clone());
    }
    Ok((plan, rows, fields_by_rung))
}

fn lane_label(rung: &str, row_name: &str) -> String {
    if rung == row_name {
        rung.to_string()
    } else {
        format!("{rung} {row_name}")
    }
}

fn row_capacity(row: &Value, harness_detail: Option<&Value>) -> (String, String) {
    let account = row_value(row, "account");
    let detail = match harness_detail {
        Some(d) => d,
        None => return ("unknown".to_string(), String::new()),
    };
    // A capacity entry is either the detailed mapping runtime_capacity
    // produces or a bare state string; both are admitted. A named account
    // missing from the detail reads unknown, never the harness-wide best.
    if account.is_empty() {
        let (state, window) = match detail.as_object() {
            Some(obj) => (
                obj.get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                obj.get("window").and_then(Value::as_str).unwrap_or(""),
            ),
            None => (detail.as_str().unwrap_or("unknown"), ""),
        };
        let mut s = state.trim().to_lowercase();
        if s.is_empty() {
            s = "unknown".to_string();
        }
        (s, window.to_string())
    } else {
        let mut state = detail
            .get("accounts")
            .and_then(|a| a.get(&account))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .trim()
            .to_lowercase();
        if state.is_empty() {
            state = "unknown".to_string();
        }
        let window = detail
            .get("window")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (state, window)
    }
}

fn candidate_supported(
    harness: &str,
    substrate: Option<&str>,
    permission_mode: Option<&str>,
    thread_seatable: &Value,
) -> bool {
    let mut sub = substrate.unwrap_or("").trim().to_string();
    if sub == "bg" {
        sub = "thread".to_string();
    }
    if sub == "thread" {
        match thread_seatable.get(harness).and_then(Value::as_bool) {
            Some(false) => return false,
            _ => {} // missing entry degrades open; the spawn gate owns refusal
        }
    }
    let mode = permission_mode.unwrap_or("").trim();
    if !mode.is_empty() && harness != "claude" && !sub.is_empty() && sub != "pane" {
        return false;
    }
    true
}

fn allowed(value: &str, enum_values: &[&str]) -> bool {
    enum_values.contains(&value)
}

const BAND_RANK_KEYS: [&str; 4] = ["low", "medium", "high", "max"];

/// A row's strength rank; a band outside the vocabulary (including unbanded)
/// ranks -1, below every banded row.
fn band_rank(band: &str) -> i64 {
    BAND_RANK_KEYS
        .iter()
        .position(|k| *k == band)
        .map(|i| i as i64)
        .unwrap_or(-1)
}

/// One grid/tier candidate row as the payload's `inventory.rows` array carries
/// it (declared order preserved by the caller).
#[derive(Clone)]
struct InvRow {
    name: String,
    harness: String,
    model: String,
    band: String,
    percentile: Option<f64>,
    cost: Option<f64>,
    effort: String,
    raw: Value,
}

fn inv_row(v: &Value) -> InvRow {
    let band = row_value(v, "band").to_lowercase();
    InvRow {
        name: row_value(v, "name"),
        harness: row_value(v, "harness"),
        model: row_value(v, "model"),
        percentile: v.get("percentile").and_then(Value::as_f64),
        cost: v.get("cost_per_mtok_in").and_then(Value::as_f64),
        effort: row_value(v, "effort"),
        band,
        raw: v.clone(),
    }
}

/// The declared objective orders candidates; it never lowers a band. Ties
/// break on name so the order is a fact, not an accident.
fn order_candidates(mut rows: Vec<InvRow>, objective: &str, prefer_harness: &str) -> Vec<InvRow> {
    // Python's `-(pct or -1.0)`: a missing percentile and a 0.0 both read
    // as -1.0, so a weakest-snapshot row never outranks an unnamed one.
    let pct = |r: &InvRow| match r.percentile {
        None | Some(0.0) => -1.0f64,
        Some(p) => p,
    };
    match objective {
        "best-available" => rows.sort_by(|a, b| {
            band_rank(&b.band)
                .cmp(&band_rank(&a.band))
                .then(pct(b).total_cmp(&pct(a)))
                .then(a.name.cmp(&b.name))
        }),
        "prefer-harness" => rows.sort_by(|a, b| {
            let pa: i32 = if a.harness == prefer_harness { 0 } else { 1 };
            let pb: i32 = if b.harness == prefer_harness { 0 } else { 1 };
            pa.cmp(&pb)
                .then(band_rank(&b.band).cmp(&band_rank(&a.band)))
                .then(pct(b).total_cmp(&pct(a)))
                .then(a.name.cmp(&b.name))
        }),
        // cheapest-that-clears: declared cost first, the percentile proxy
        // second, then the weakest-clearing rule.
        _ => rows.sort_by(|a, b| {
            let group = |r: &InvRow| {
                if r.cost.is_some() {
                    0
                } else if r.percentile.is_some() {
                    1
                } else {
                    2
                }
            };
            let (ga, gb) = (group(a), group(b));
            if ga != gb {
                return ga.cmp(&gb);
            }
            let inner = |x: &InvRow, y: &InvRow| -> std::cmp::Ordering {
                if let (Some(ca), Some(cb)) = (x.cost, y.cost) {
                    return ca.total_cmp(&cb);
                }
                if let (Some(pa), Some(pb)) = (x.percentile, y.percentile) {
                    return pa.total_cmp(&pb);
                }
                std::cmp::Ordering::Equal
            };
            inner(a, b)
                .then(band_rank(&a.band).cmp(&band_rank(&b.band)))
                .then(a.name.cmp(&b.name))
        }),
    }
    rows
}

/// The no-lanes fallthrough: difficulty and priority join the declared
/// inventory under a live capacity snapshot. Receipts are the Python
/// resolver's, verbatim.
fn grid_leg(payload: &Value, rung_base: &str, chain: &mut Vec<Value>) -> Value {
    let capacity = payload.get("capacity").cloned().unwrap_or(json!({}));
    let node = payload.get("node").cloned().unwrap_or(Value::Null);
    let band_raw = node
        .get("difficulty")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let band = if BAND_RANK_KEYS.contains(&band_raw.as_str()) {
        band_raw
    } else {
        // Round up under uncertainty: the failure is asymmetric.
        "high".to_string()
    };
    let prio = node
        .get("priority")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "p2".to_string());
    chain.push(json!(format!("grid difficulty({band}) priority({prio})")));
    if !["p0", "p1", "p2", "p3"].contains(&prio.as_str()) {
        chain.push(json!("grid=invalid-input"));
        return none(chain.clone());
    }
    let inventory = payload.get("inventory").cloned().unwrap_or(json!({}));
    let declared = inventory
        .get("declared")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let inv_rows: Vec<Value> = inventory
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !declared || inv_rows.is_empty() {
        chain.push(json!("grid=no-inventory-declared"));
        return none(chain.clone());
    }
    // p0 bills at the strong band, p3 prefers the cheap one; the planning
    // role floors at the strong end because a plan earns the cheap tier.
    let mut candidate_band = if prio == "p0" {
        "high".to_string()
    } else if prio == "p3" {
        "low".to_string()
    } else {
        band.clone()
    };
    let mut objective = inventory
        .get("objective")
        .and_then(Value::as_str)
        .unwrap_or("cheapest-that-clears")
        .to_string();
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    if role == "planning" {
        candidate_band = "high".to_string();
        chain.push(json!("grid role(planning) floors band(high)"));
    }
    let protected_role = payload
        .get("protected_role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(prot) = protected_role {
        let floor = payload
            .get("protected_role_floor")
            .and_then(Value::as_str)
            .unwrap_or("high");
        if band_rank(&candidate_band) < band_rank(floor) {
            candidate_band = floor.to_string();
        }
        objective = "best-available".to_string();
        chain.push(json!(format!("grid protected-role({prot}) floor={floor}")));
    }
    let substrate = payload.get("substrate").and_then(Value::as_str);
    let permission_mode = payload.get("permission_mode").and_then(Value::as_str);
    let constrain = payload
        .get("constrain_harness")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut rows: Vec<InvRow> = Vec::new();
    for raw in &inv_rows {
        let r = inv_row(raw);
        if let Some(h) = constrain {
            if r.harness != h {
                continue;
            }
        }
        rows.push(r);
    }
    if let Some(h) = constrain {
        chain.push(json!(format!("grid constrained to harness({h})")));
    }
    let before_filters = rows.len();
    let thread_seatable = payload.get("thread_seatable").cloned().unwrap_or(json!({}));
    rows.retain(|r| candidate_supported(&r.harness, substrate, permission_mode, &thread_seatable));
    if substrate.map_or(false, |s| !s.trim().is_empty())
        || permission_mode.map_or(false, |s| !s.trim().is_empty())
    {
        if rows.is_empty() && before_filters > 0 {
            chain.push(json!("grid=constrained-empty"));
            return none(chain.clone());
        }
        chain.push(json!(format!(
            "grid filtered by substrate({}) permission({})",
            substrate.unwrap_or("-"),
            permission_mode.unwrap_or("-"),
        )));
    }
    // A declared row whose harness fno cannot drive REFUSES by name; it is a
    // fact the receipt carries, never a silent skip.
    let installed = payload
        .get("harness_installed")
        .cloned()
        .unwrap_or(json!({}));
    let mut keep: Vec<InvRow> = Vec::new();
    for r in rows {
        let ok = r.harness.is_empty()
            || r.model.is_empty()
            || installed
                .get(&r.harness)
                .and_then(Value::as_bool)
                .unwrap_or(true);
        if ok {
            keep.push(r);
        } else {
            chain.push(json!(format!(
                "grid refuses {}: harness '{}' not installed",
                r.name, r.harness
            )));
        }
    }
    let floor_rank = band_rank(&candidate_band);
    let mut clearing: Vec<InvRow> = keep
        .iter()
        .filter(|r| {
            band_rank(&r.band) >= floor_rank && !r.harness.is_empty() && !r.model.is_empty()
        })
        .cloned()
        .collect();
    let unbanded: Vec<InvRow> = keep
        .iter()
        .filter(|r| r.band.is_empty() && !r.harness.is_empty() && !r.model.is_empty())
        .cloned()
        .collect();
    if clearing.is_empty() && unbanded.is_empty() {
        chain.push(json!("grid=no-band-candidate"));
        return none(chain.clone());
    }
    let prefer = inventory
        .get("prefer_harness")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    clearing = order_candidates(clearing, &objective, &prefer);
    let effort_ok = payload.get("effort_ok").cloned().unwrap_or(json!({}));
    for row in clearing.iter().chain(unbanded.iter()) {
        let detail = capacity.get(&row.harness);
        let (mut state, window) = row_capacity(&row.raw, detail);
        if state == "exhausted" || state == "blocked" {
            chain.push(json!(format!(
                "grid skip {}/{} capacity={state}",
                row.harness, row.name
            )));
            continue;
        }
        if state != "ok" && state != "low" && state != "available" {
            state = "unknown-permitted".to_string();
        }
        let mut line = format!(
            "grid candidate {}/{} capacity={state}",
            row.harness, row.name
        );
        if !window.is_empty() {
            line.push_str(&format!(" window={window}"));
        }
        if row.band.is_empty() {
            line.push_str(" band=unbanded");
        }
        chain.push(json!(line));
        let mut out = Map::new();
        out.insert("harness".into(), json!(row.harness));
        out.insert("model".into(), json!(row.model));
        // Same shape as the lane leg's candidate (pick): route and account are
        // facts the declared row already carries; a grid candidate born without
        // them sends a vendorless argv to the spawn gate.
        let route = row_value(&row.raw, "route");
        let account = row_value(&row.raw, "account");
        if !route.is_empty() {
            out.insert("route".into(), json!(route));
        }
        if !account.is_empty() {
            out.insert("account".into(), json!(account));
        }
        let mut effort = row.effort.clone();
        if !effort.is_empty() {
            let valid = effort_ok
                .get(&row.harness)
                .and_then(|m| m.get(&effort))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !valid {
                chain.push(json!(format!(
                    "grid effort omitted (no surface on {})",
                    row.harness
                )));
                effort = String::new();
            }
        }
        if !effort.is_empty() {
            out.insert("effort".into(), json!(effort));
            chain.push(json!(format!("grid effort({effort})")));
        }
        return json!({
            "status": "pick",
            "candidate": Value::Object(out),
            "chain": chain,
        });
    }
    // Every candidate was skipped on a positive exhausted/blocked marker.
    chain.push(json!("grid=no-available-candidate"));
    none(chain.clone())
}

/// Tier resolution: a band to a concrete declared model, scoped to one
/// harness when asked. Degrades below the floor rather than blocking.
fn tier_leg(payload: &Value) -> Value {
    let mut chain: Vec<Value> = Vec::new();
    let band = payload
        .get("tier")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    chain.push(json!(format!("tier({band})")));
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(p) = provider {
        chain.push(json!(format!("provider({p})")));
    }
    if !BAND_RANK_KEYS.contains(&band.as_str()) {
        chain.push(json!("unknown-tier -> provider default"));
        return tier_none(chain);
    }
    let inventory = payload.get("inventory").cloned().unwrap_or(json!({}));
    let inv_rows: Vec<Value> = inventory
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if inv_rows.is_empty() {
        chain.push(json!("no declared inventory -> provider default"));
        return tier_none(chain);
    }
    let rows: Vec<InvRow> = inv_rows
        .iter()
        .map(inv_row)
        .filter(|r| {
            !r.harness.is_empty()
                && !r.model.is_empty()
                && provider.map_or(true, |p| r.harness == p)
        })
        .collect();
    let floor_rank = band_rank(&band);
    let mut clearing: Vec<InvRow> = rows
        .iter()
        .filter(|r| band_rank(&r.band) >= floor_rank)
        .cloned()
        .collect();
    let objective = inventory
        .get("objective")
        .and_then(Value::as_str)
        .unwrap_or("cheapest-that-clears");
    let prefer = inventory
        .get("prefer_harness")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !clearing.is_empty() {
        clearing = order_candidates(clearing, objective, prefer);
        let row = &clearing[0];
        chain.push(json!(format!("inventory band(>={band}) -> {}", row.name)));
        return tier_pick(&row.model, chain);
    }
    let below: Vec<InvRow> = rows
        .iter()
        .filter(|r| band_rank(&r.band) >= 0 && band_rank(&r.band) < floor_rank)
        .cloned()
        .collect();
    if let Some(best) = below.iter().max_by(|a, b| {
        let pct = |r: &InvRow| match r.percentile {
            None | Some(0.0) => -1.0f64,
            Some(p) => p,
        };
        band_rank(&a.band)
            .cmp(&band_rank(&b.band))
            .then(pct(a).total_cmp(&pct(b)))
    }) {
        chain.push(json!(format!(
            "inventory band(>={band}) empty -> degrade -> {}",
            best.name
        )));
        return tier_pick(&best.model, chain);
    }
    chain.push(json!(
        "inventory has no reachable model -> provider default"
    ));
    tier_none(chain)
}

fn tier_pick(model: &str, chain: Vec<Value>) -> Value {
    json!({"status": "pick", "model": model, "chain": chain})
}

fn tier_none(chain: Vec<Value>) -> Value {
    json!({"status": "none", "model": Value::Null, "chain": chain})
}

/// The readout leg: every planned lane's live capacity state, for display.
/// No selection, no terminal; a lane naming no row reads no-such-row.
fn states_leg(payload: &Value) -> Value {
    let rung_base = payload
        .get("rung_base")
        .and_then(Value::as_str)
        .unwrap_or("agents.profiles");
    let capacity = payload.get("capacity").cloned().unwrap_or(json!({}));
    let profile = payload.get("profile").cloned().unwrap_or(Value::Null);
    let lanes_raw = payload.get("lanes_raw").cloned().unwrap_or(json!([]));
    let mut chain: Vec<Value> = Vec::new();
    let mut lanes_raw = lanes_raw;
    let mut prefix: Vec<Value> = Vec::new();
    let mut diff_note: Option<String> = None;
    // The readout's policy lines, displayed the way the slot gate refuses on
    // them: normalized when valid, ``{raw} (invalid)`` when not. Display
    // reads the BASE profile; the walk below applies overlays to the lanes.
    let policy_raw = |name: &str, default: &str| -> String {
        match profile.get(name).and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => default.to_string(),
        }
    };
    let raw_exhausted = policy_raw("on_exhausted", "refuse");
    let norm = raw_exhausted.trim().to_lowercase();
    let on_exhausted = if ON_EXHAUSTED.contains(&norm.as_str()) {
        norm
    } else {
        format!("{raw_exhausted} (invalid)")
    };
    let on_low = policy_raw("on_low", "prefer_healthy");
    let on_unknown = policy_raw("on_unknown", "allow");
    // The lane a spawn would take right now: the same walk a dispatch runs,
    // node-less, so the preview can never disagree with the gate.
    let walk_verdict = |payload: &Value| -> (Value, &'static str, Value) {
        let mut slot_payload = payload.clone();
        if let Some(obj) = slot_payload.as_object_mut() {
            obj.remove("mode");
        }
        let slot_out = resolve_slot_payload(&slot_payload);
        let candidate = slot_out.get("candidate");
        let lane_rung = candidate
            .and_then(|c| c.get("lane_rung"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // The verdict names WHY nothing is placed: a policy refusal is a
        // hold, never "exhausted", and capacity is held, not broken.
        let routing = if !lane_rung.is_empty() {
            "armed"
        } else {
            match slot_out.get("reason_kind").and_then(Value::as_str) {
                Some("policy-refusal") => "policy-held",
                Some("capacity-queue") | Some("capacity-exhausted") => "capacity-held",
                _ => "unarmed",
            }
        };
        let mut facts = json!({});
        if let Some(obj) = facts.as_object_mut() {
            let pol = candidate.and_then(|c| c.get("policy"));
            let get = |k: &str| {
                pol.and_then(|p| p.get(k))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let (wk, oa, src) = (get("work_kind"), get("operator_access"), get("source"));
            // On a hold the decision carries no candidate policy: surface the
            // access filter from the payload's own policy block instead, so a
            // held readout still names the access that held it.
            let (oa, src) = if oa.is_empty() {
                (
                    payload
                        .get("policy")
                        .and_then(|p| p.get("operator_access"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    if payload
                        .get("policy")
                        .and_then(|p| p.get("enforce_inventory"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        "config routing.enforce_inventory".to_string()
                    } else {
                        String::new()
                    },
                )
            } else {
                (oa, src)
            };
            if !wk.is_empty() {
                obj.insert("work_kind".into(), json!(wk));
            }
            if !oa.is_empty() {
                obj.insert("operator_access".into(), json!(oa));
                if !src.is_empty() {
                    obj.insert("policy_source".into(), json!(src));
                }
            }
            let skips: Vec<Value> = slot_out
                .get("chain")
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter(|l| l.as_str().map_or(false, |s| s.starts_with("slot skip ")))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !skips.is_empty() {
                obj.insert("skipped".into(), Value::Array(skips));
            }
        }
        let would_take = if !lane_rung.is_empty() {
            let lane = candidate
                .and_then(|c| c.get("lane"))
                .and_then(Value::as_str)
                .unwrap_or("");
            json!(format!("{lane_rung} {lane}"))
        } else {
            slot_out
                .get("chain")
                .and_then(Value::as_array)
                .and_then(|c| c.last())
                .cloned()
                .unwrap_or(json!(""))
        };
        (would_take, routing, facts)
    };
    let (would_take, routing, facts) = walk_verdict(payload);
    let with_facts = |mut out: Value| -> Value {
        if let (Some(obj), Some(f)) = (out.as_object_mut(), facts.as_object()) {
            for (k, v) in f {
                obj.insert(k.clone(), v.clone());
            }
        }
        out
    };
    if let Some(bd) = profile
        .get("by_difficulty")
        .and_then(Value::as_object)
        .filter(|bd| !bd.is_empty())
    {
        let (diff_key, diff_reason) = {
            let node_difficulty = payload
                .get("node")
                .and_then(|n| n.get("difficulty"))
                .and_then(Value::as_str);
            effective_difficulty(node_difficulty)
        };
        diff_note = diff_reason;
        if let Some(ovl) = bd.get(&diff_key).and_then(Value::as_object) {
            for key in ovl.keys() {
                if !["lanes", "on_exhausted", "on_low", "on_unknown"].contains(&key.as_str()) {
                    chain.push(json!(format!(
                        "slot=config {rung_base}.by_difficulty.{diff_key} has unknown field {key:?}"
                    )));
                    return with_facts(json!({
                        "status": "states", "lane_states": [], "chain": chain,
                        "on_exhausted": on_exhausted, "on_low": on_low,
                        "on_unknown": on_unknown,
                        "would_take": would_take, "routing": routing,
                    }));
                }
            }
            if let Some(ovl_lanes) = ovl.get("lanes") {
                match ovl_lanes.as_array() {
                    Some(arr) if !arr.is_empty() => lanes_raw = Value::Array(arr.clone()),
                    _ => {
                        chain.push(json!(format!(
                            "slot=config {rung_base}.by_difficulty.{diff_key}.lanes must be a non-empty list when declared"
                        )));
                        return with_facts(json!({
                            "status": "states", "lane_states": [],
                            "chain": chain,
                            "on_exhausted": on_exhausted, "on_low": on_low,
                            "on_unknown": on_unknown,
                            "would_take": would_take, "routing": routing,
                        }));
                    }
                }
            }
        }
    }
    let lanes_arr = lanes_raw.as_array().cloned().unwrap_or_default();
    if lanes_arr.is_empty() {
        // The no-lanes verdict: what a spawn would fall back to. No walk ran,
        // so the policy lines stay unset exactly like an unarmed slot.
        let inv = payload.get("inventory").cloned().unwrap_or(json!({}));
        let rows = inv.get("rows").and_then(Value::as_array);
        let would_take = if inv
            .get("declared")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && rows.map_or(false, |r| !r.is_empty())
        {
            json!(format!(
                "no lanes; grid over {} rows",
                rows.map_or(0, |r| r.len())
            ))
        } else {
            json!("no lanes; no inventory; harness default")
        };
        return with_facts(json!({
            "status": "states", "lane_states": [], "chain": chain,
            "would_take": would_take, "routing": routing,
        }));
    }
    if let Some(reason) = diff_note {
        prefix.push(json!(format!("slot note {rung_base} {reason}")));
    }
    let declared_rows = payload
        .get("declared_rows")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let (plan, rows, _fields) = match fold(rung_base, &lanes_arr, &declared_rows) {
        Ok(f) => f,
        Err(line) => {
            chain.push(json!(line));
            return with_facts(json!({
                "status": "states", "lane_states": [], "chain": chain,
                "on_exhausted": on_exhausted, "on_low": on_low,
                "on_unknown": on_unknown,
                "would_take": would_take, "routing": routing,
            }));
        }
    };
    let mut lane_states = Vec::new();
    for (rung, row_name) in &plan {
        let row = rows.get(row_name);
        let (state, window, identity, source) = match row {
            None => ("no-such-row".to_string(), String::new(), None, None),
            Some(r) => {
                let harness = row_value(r, "harness");
                let account = row_value(r, "account");
                let route = row_value(r, "route");
                let detail = capacity.get(&harness);
                let (s, w) = row_capacity(r, detail);
                // Display evidence: the attribution owner's verdict for the
                // row's named account, and where the observation came from.
                let ident = if !account.is_empty() && route.is_empty() {
                    Some(
                        detail
                            .and_then(|d| d.get("evidence"))
                            .and_then(|e| e.get(&account))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                    )
                } else {
                    None
                };
                let src = detail
                    .and_then(|d| d.get("window"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                (s, w, ident, src)
            }
        };
        let mut entry = json!({
            "rung": rung,
            "name": row_name,
            "state": state,
            "window": window,
        });
        if let (Some(obj), Some(ident)) = (entry.as_object_mut(), identity) {
            obj.insert("identity".into(), json!(ident));
        }
        if let (Some(obj), Some(src)) = (entry.as_object_mut(), source) {
            obj.insert("source".into(), json!(src));
        }
        lane_states.push(entry);
    }
    chain.extend(prefix);
    with_facts(json!({
        "status": "states",
        "lane_states": lane_states,
        "chain": chain,
        "on_exhausted": on_exhausted,
        "on_low": on_low,
        "on_unknown": on_unknown,
        "would_take": would_take,
        "routing": routing,
    }))
}

/// The resolver core: payload in, `{status, candidate, chain}` out.
pub fn resolve_slot_payload(payload: &Value) -> Value {
    let mut chain: Vec<Value> = Vec::new();
    let mode = payload.get("mode").and_then(Value::as_str).unwrap_or("");
    if mode == "tier" {
        return tier_leg(payload);
    }
    if mode == "states" {
        return states_leg(payload);
    }
    let rung_base = payload
        .get("rung_base")
        .and_then(Value::as_str)
        .unwrap_or("agents.profiles")
        .to_string();
    let profile = payload.get("profile").cloned().unwrap_or(Value::Null);
    let capacity = payload.get("capacity").cloned().unwrap_or(json!({}));
    let gate_bypassed = payload
        .get("gate_bypassed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let explicit_lane = payload
        .get("explicit_lane")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let substrate = payload.get("substrate").and_then(Value::as_str);
    let permission_mode = payload.get("permission_mode").and_then(Value::as_str);
    let constrain_harness = payload
        .get("constrain_harness")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let thread_seatable = payload.get("thread_seatable").cloned().unwrap_or(json!({}));

    // --- strict inventory policy ---------------------------------------------
    // When routing.enforce_inventory is set, the effective work kind picks
    // WHICH declared slot this dispatch walks, and only that slot's
    // CONFIG-declared lanes can answer. The grid, the built-in fallback and
    // the harness default are all out of the decision path; a request the
    // slot cannot answer is a named refusal, never an ambient default.
    let strict_policy = payload.get("policy").cloned().unwrap_or(json!({}));
    let enforce = strict_policy
        .get("enforce_inventory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut strict_ctx: Option<(String, String)> = None; // (operator_access, slot verb)
    let mut rung_base = rung_base;
    let mut profile = profile;
    let mut lanes_raw_value = payload.get("lanes_raw").cloned().unwrap_or(json!([]));
    if enforce {
        let operator_access = strict_policy
            .get("operator_access")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        if !["local", "remote", "unknown"].contains(&operator_access.as_str()) {
            chain.push(json!(format!(
                "slot=config routing.operator_access {operator_access:?} is not local|remote|unknown"
            )));
            return refused_decision(
                chain,
                "policy-config-invalid",
                "routing.operator_access is not local|remote|unknown",
            );
        }
        let work_verb = payload
            .get("work_verb")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| rung_base.trim_start_matches("agents.profiles.").to_string());
        let plan_path = payload
            .get("node")
            .and_then(|n| n.get("plan_path"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let (slot_verb, note) = effective_work_kind(&work_verb, !plan_path.trim().is_empty());
        if let Some(note) = note {
            chain.push(json!(note));
        }
        let slot = payload
            .get("slot_by_verb")
            .and_then(Value::as_object)
            .and_then(|s| s.get(slot_verb.as_str()));
        match slot {
            Some(s) => {
                rung_base = s
                    .get("rung_base")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .unwrap_or(&format!("agents.profiles.{slot_verb}"))
                    .to_string();
                profile = s.get("profile").cloned().unwrap_or(json!({}));
                lanes_raw_value = s.get("lanes_raw").cloned().unwrap_or(json!([]));
            }
            None => {
                rung_base = format!("agents.profiles.{slot_verb}");
                profile = json!({});
                lanes_raw_value = json!([]);
            }
        }
        strict_ctx = Some((operator_access, slot_verb));
    }
    let by_difficulty = profile.get("by_difficulty").cloned().unwrap_or(json!({}));
    let by_difficulty_obj = by_difficulty.as_object();

    if !by_difficulty.is_null() && by_difficulty_obj.is_none() {
        chain.push(json!(format!(
            "slot=config {rung_base}.by_difficulty must be a table"
        )));
        return none(chain);
    }
    if let Some(bd) = by_difficulty_obj {
        for key in bd.keys() {
            if !DIFFICULTY_KEYS.contains(&key.as_str()) {
                chain.push(json!(format!(
                    "slot=config {rung_base}.by_difficulty key {key:?} is not low|medium|high"
                )));
                return none(chain);
            }
        }
    }

    let mut lanes_raw = lanes_raw_value;
    let mut prefix: Vec<Value> = Vec::new();
    let mut overlay_fields: Map<String, Value> = Map::new();
    if let Some(bd) = by_difficulty_obj.filter(|bd| !bd.is_empty()) {
        let (diff_key, diff_reason) = {
            let node_difficulty = payload
                .get("node")
                .and_then(|n| n.get("difficulty"))
                .and_then(Value::as_str);
            effective_difficulty(node_difficulty)
        };
        let overlay = bd.get(&diff_key);
        let overlay_obj = overlay.and_then(Value::as_object);
        if let Some(ovl) = overlay_obj {
            for key in ovl.keys() {
                if !["lanes", "on_exhausted", "on_low", "on_unknown"].contains(&key.as_str()) {
                    chain.push(json!(format!(
                        "slot=config {rung_base}.by_difficulty.{diff_key} has unknown field {key:?}"
                    )));
                    return none(chain);
                }
            }
            if let Some(ovl_lanes) = ovl.get("lanes") {
                match ovl_lanes.as_array() {
                    Some(arr) if !arr.is_empty() => lanes_raw = Value::Array(arr.clone()),
                    _ => {
                        chain.push(json!(format!(
                            "slot=config {rung_base}.by_difficulty.{diff_key}.lanes must be a non-empty list when declared"
                        )));
                        return none(chain);
                    }
                }
            }
            overlay_fields = ovl.clone();
        }
        if let Some(reason) = diff_reason {
            prefix.push(json!(format!("slot note {rung_base} {reason}")));
        }
    }

    let lanes_arr = lanes_raw.as_array().cloned().unwrap_or_default();
    let lanes_is_list = lanes_raw.is_array() || lanes_raw.is_null();
    if !lanes_is_list {
        chain.push(json!(format!(
            "slot=config {rung_base}.lanes must be a list; got {lanes_raw}"
        )));
        return none(chain);
    }

    chain.extend(prefix);
    if lanes_arr.is_empty() {
        // A lane-less verb grids instead: the model axis reads occupied there
        // and the grid stands down; an explicit model pin never reaches the
        // lanes here, so the grid's own occupied flag governs. Strict routing
        // has no grid: an empty effective slot is the named refusal.
        if strict_ctx.is_some() {
            chain.push(json!(format!(
                "slot=strict-refusal slot {rung_base} declares no lanes; strict routing refuses the harness default"
            )));
            return refused_decision(
                chain,
                "policy-no-declared-slot",
                "the effective work-kind slot declares no lanes",
            );
        }
        chain.push(json!(format!(
            "slot {rung_base} has no lanes; grid over inventory"
        )));
        if payload
            .get("model_occupied")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            chain.push(json!("grid=model-axis-occupied"));
            return none(chain);
        }
        return grid_leg(&payload, &rung_base, &mut chain);
    }

    // An explicit model pin outranks the lanes (operator authority); it never
    // borrows a lane's harness or capacity. Config defaults do NOT outrank
    // lanes; only a typed flag does. Strict routing instead qualifies the
    // explicit coordinate against the effective slot's membership.
    if strict_ctx.is_none()
        && payload
            .get("explicit_model")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        chain.push(json!(
            "slot=model-pin-override (an explicit model outranks the lanes)"
        ));
        return none(chain);
    }
    chain.push(json!(format!(
        "slot {rung_base} lanes walked in declared order"
    )));

    // Policy resolution with named refusals; an out-of-enum value is a
    // config fault, never a silent permissive coercion. The selected
    // overlay's policy field outranks the base profile's; both validated.
    let mut policy = |name: &str, default: &str, enum_values: &[&str]| -> Option<String> {
        let overlay_value = overlay_fields.get(name);
        let raw = overlay_value
            .or_else(|| profile.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_lowercase();
        if allowed(&raw, enum_values) {
            Some(raw)
        } else {
            chain.push(json!(format!(
                "slot=config {rung_base}.{name} {raw:?} is not one of {}",
                enum_values.join("|")
            )));
            None
        }
    };
    let on_exhausted = match policy("on_exhausted", "refuse", &ON_EXHAUSTED) {
        Some(v) => v,
        None => return none(chain),
    };
    let on_low = match policy("on_low", "prefer_healthy", &ON_LOW) {
        Some(v) => v,
        None => return none(chain),
    };
    let on_unknown = match policy("on_unknown", "allow", &ON_UNKNOWN) {
        Some(v) => v,
        None => return none(chain),
    };

    let declared_rows = payload
        .get("declared_rows")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let (plan, rows, fields_by_rung) = match fold(&rung_base, &lanes_arr, &declared_rows) {
        Ok(f) => f,
        Err(line) => {
            chain.push(json!(line));
            return none(chain);
        }
    };
    let sorted_rows: Vec<String> = {
        let mut names: Vec<String> = rows.keys().cloned().collect();
        names.sort();
        names
    };

    let vendor_counts = payload
        .get("vendor_counts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let vendor_count_errors = payload
        .get("vendor_count_errors")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let vendor_caps = payload
        .get("vendor_caps")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Strict: an explicit coordinate is a CONSTRAINT on the slot's membership,
    // never a bypass. The walk keeps only the lanes naming that exact
    // coordinate; a coordinate no row names is the named refusal.
    let mut plan = plan;
    if strict_ctx.is_some() {
        let explicit_model_name = payload
            .get("explicit_model_value")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let explicit_route_name = payload
            .get("explicit_route_value")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(m) = &explicit_model_name {
            let member = plan.iter().any(|(_, rn)| {
                rows.get(rn).map(|r| row_value(r, "model")).as_deref() == Some(m.as_str())
            });
            if !member {
                chain.push(json!(format!(
                    "slot=strict-refusal explicit model {m:?} is not in slot {rung_base}'s declared lanes"
                )));
                return refused_decision(
                    chain,
                    "policy-coordinate-not-in-slot",
                    "the explicit model is not in the effective slot's declared lanes",
                );
            }
        }
        if let Some(rt) = &explicit_route_name {
            let member = plan.iter().any(|(_, rn)| {
                rows.get(rn).map(|r| row_value(r, "route")).as_deref() == Some(rt.as_str())
            });
            if !member {
                chain.push(json!(format!(
                    "slot=strict-refusal explicit route {rt:?} is not in slot {rung_base}'s declared lanes"
                )));
                return refused_decision(
                    chain,
                    "policy-coordinate-not-in-slot",
                    "the explicit route is not in the effective slot's declared lanes",
                );
            }
        }
        if explicit_model_name.is_some() || explicit_route_name.is_some() {
            plan.retain(|(_, rn)| {
                let r = rows.get(rn);
                let model_ok = explicit_model_name
                    .as_ref()
                    .map(|m| r.map(|row| row_value(row, "model")).as_deref() == Some(m.as_str()));
                let route_ok = explicit_route_name
                    .as_ref()
                    .map(|rt| r.map(|row| row_value(row, "route")).as_deref() == Some(rt.as_str()));
                model_ok.unwrap_or(true) && route_ok.unwrap_or(true)
            });
        }
    }

    let mut demoted: Vec<(usize, String, String, String)> = Vec::new();
    let mut identity_skips: Vec<String> = Vec::new();
    let mut policy_skips: usize = 0;
    let mut resets_seen: Vec<f64> = Vec::new();

    for (index, (rung, row_name)) in plan.iter().enumerate() {
        let row = match rows.get(row_name) {
            Some(r) => r.clone(),
            None => {
                let declared = if sorted_rows.is_empty() {
                    "(none)".to_string()
                } else {
                    sorted_rows.join(", ")
                };
                chain.push(json!(format!(
                    "slot=config {rung} names no [[routing.models]] row '{row_name}'; declared rows: {declared}; see fno config route inventory"
                )));
                return none(chain);
            }
        };
        let harness = row_value(&row, "harness");
        if !candidate_supported(&harness, substrate, permission_mode, &thread_seatable) {
            chain.push(json!(format!(
                "slot skip {} harness {harness:?} cannot carry substrate({}) permission({})",
                lane_label(rung, row_name),
                substrate.unwrap_or("-"),
                permission_mode.unwrap_or("-"),
            )));
            continue;
        }
        let route = row_value(&row, "route");
        let account = row_value(&row, "account");
        // Strict: native view qualification. Under remote or unknown, only a
        // row whose operator_view names the harness's native view qualifies; a
        // label that contradicts the row's own coordinate refuses outright.
        if let Some((access, _slot_v)) = &strict_ctx {
            let view = row_value(&row, "operator_view");
            let native = native_view_for(&harness);
            // A native view label must name the harness's native view AND a
            // row with no vendor route: a vendor lane is by definition not the
            // native coordinate, so the label contradicts the row itself.
            let contradictory =
                !view.is_empty() && (native != Some(view.as_str()) || !route.is_empty());
            if contradictory {
                chain.push(json!(format!(
                    "slot=config {rung} row '{row_name}' labels operator_view={view:?} but its coordinate (harness {harness:?}, route {route:?}) is not that native view"
                )));
                return refused_decision(
                    chain,
                    "policy-view-contradiction",
                    "an operator_view label contradicts the row's own harness/route coordinate",
                );
            }
            if access != "local" {
                match native {
                    None => {
                        chain.push(json!(format!(
                            "slot skip {} no native view kind for harness {harness:?} (operator_access={access})",
                            lane_label(rung, row_name),
                        )));
                        policy_skips += 1;
                        continue;
                    }
                    Some(nv) if view != nv => {
                        chain.push(json!(format!(
                            "slot skip {} no verified native view (operator_access={access})",
                            lane_label(rung, row_name),
                        )));
                        policy_skips += 1;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        let vendor = route
            .split(',')
            .next()
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let vendor = if route.is_empty() { None } else { Some(vendor) };
        if let Some(vendor_name) = vendor.as_deref() {
            if !account.is_empty() {
                // The record the account names resolves its OWN launch env; a
                // lane route contradicting that vendor would check one
                // coordinate and bill another, so it refuses before anything
                // launches.
                if let Some(rec_vendor) = payload
                    .get("account_record_vendors")
                    .and_then(|v| v.get(&account))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    if rec_vendor != vendor_name {
                        chain.push(json!(format!(
                            "slot=config {rung} account '{account}' resolves vendor '{rec_vendor}', contradicting the lane route '{route}'"
                        )));
                        return none(chain);
                    }
                }
            }
        }
        let cap = vendor
            .as_deref()
            .and_then(|v| vendor_caps.get(v))
            .and_then(Value::as_u64);
        if let (Some(vendor), Some(cap)) = (vendor.as_deref(), cap) {
            if let Some(err) = vendor_count_errors.get(vendor).and_then(Value::as_str) {
                if gate_bypassed {
                    chain.push(json!(format!(
                        "slot note {} {vendor} count unavailable ({err}); FNO_SPAWN_GATE=0 keeps the lane",
                        lane_label(rung, row_name),
                    )));
                } else {
                    chain.push(json!(format!(
                        "slot=provider-count-unavailable {rung} {vendor}: {err}"
                    )));
                    return none(chain);
                }
            } else {
                let current = vendor_counts
                    .get(vendor)
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if current >= cap {
                    chain.push(json!(format!(
                        "slot skip {} provider {vendor} at {current} of {cap}",
                        lane_label(rung, row_name),
                    )));
                    continue;
                }
            }
        }
        let harness_detail = capacity.get(&harness);
        let evidence = harness_detail
            .and_then(|d| d.get("evidence"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !account.is_empty() && route.is_empty() {
            // A slot-account claim is governed by the attribution owner's
            // verdict; a vendor route (an API lane) never claimed the slot.
            let ident = evidence
                .get(&account)
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match ident {
                "mismatch" => {
                    identity_skips.push(rung.clone());
                    chain.push(json!(format!(
                        "slot skip {} account_identity_mismatch",
                        lane_label(rung, row_name),
                    )));
                    continue;
                }
                "unknown" if on_unknown == "skip" => {
                    identity_skips.push(rung.clone());
                    chain.push(json!(format!(
                        "slot skip {} account_identity_unknown (on_unknown=skip)",
                        lane_label(rung, row_name),
                    )));
                    continue;
                }
                _ => {}
            }
        }
        if let Some(pin) = constrain_harness {
            if harness != pin {
                chain.push(json!(format!(
                    "slot skip {} harness {harness:?} is not the pinned {pin:?}",
                    lane_label(rung, row_name),
                )));
                continue;
            }
        }
        let (mut state, window) = row_capacity(&row, harness_detail);
        if state == "exhausted" || state == "blocked" {
            if let Some(resets) = harness_detail
                .and_then(|d| d.get("resets"))
                .and_then(Value::as_object)
            {
                let candidates: Vec<f64> = resets
                    .iter()
                    .filter(|(k, _)| account.is_empty() || k.as_str() == account)
                    .filter_map(|(_, v)| v.as_f64())
                    .collect();
                if let Some(min) = candidates.iter().cloned().reduce(f64::min) {
                    resets_seen.push(min);
                }
            }
            chain.push(json!(format!(
                "slot skip {} capacity={state}",
                lane_label(rung, row_name),
            )));
            continue;
        }
        if state == "low" && on_low == "skip" {
            chain.push(json!(format!(
                "slot skip {} capacity=low (on_low=skip)",
                lane_label(rung, row_name),
            )));
            continue;
        }
        if state != "ok" && state != "low" && state != "available" {
            if on_unknown == "skip" {
                chain.push(json!(format!(
                    "slot skip {} capacity={state} (on_unknown=skip)",
                    lane_label(rung, row_name),
                )));
                continue;
            }
            state = "unknown-permitted".to_string();
        }
        if state == "low" && on_low == "prefer_healthy" {
            demoted.push((index, rung.clone(), row_name.clone(), window.clone()));
            chain.push(json!(format!(
                "slot demote {} capacity=low (on_low=prefer_healthy)",
                lane_label(rung, row_name),
            )));
            continue;
        }
        return pick(
            &mut chain,
            &rows,
            &fields_by_rung,
            &capacity,
            index,
            rung,
            row_name,
            &state,
            &window,
            "",
            strict_ctx.as_ref(),
        );
    }

    if let Some((index, rung, row_name, window)) = demoted.first().cloned() {
        return pick(
            &mut chain,
            &rows,
            &fields_by_rung,
            &capacity,
            index,
            &rung,
            &row_name,
            "low",
            &window,
            "no healthy lane; on_low=prefer_healthy",
            strict_ctx.as_ref(),
        );
    }

    // Strict: every lane in the effective slot was refused on policy alone
    // (no verified native view, or a harness with no native view kind). That
    // is a refusal naming its boundary, not a capacity queue with a fake
    // reset time.
    if let Some((access, _slot_v)) = &strict_ctx {
        if policy_skips > 0 && policy_skips == plan.len() {
            chain.push(json!(format!(
                "slot=strict-refusal every lane in {rung_base} lacked the required operator view (operator_access={access})"
            )));
            return refused_decision(
                chain,
                "policy-no-qualified-lane",
                "the operator_access filter left no lane in the effective slot",
            );
        }
    }

    if explicit_lane || gate_bypassed {
        let why = if explicit_lane {
            "the command line already names the lane"
        } else {
            "FNO_SPAWN_GATE=0"
        };
        chain.push(json!(format!("slot=exhausted degrade ({why})")));
        return none(chain);
    }
    if !identity_skips.is_empty() && identity_skips.len() == plan.len() {
        // Every lane lost on identity alone: the fix is the operator's manual
        // canonical swap, never a login performed by fno.
        chain.push(json!("slot=manual_account_switch_required"));
        return none(chain);
    }
    if on_exhausted == "queue" {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let future = resets_seen
            .iter()
            .cloned()
            .filter(|r| *r > now)
            .reduce(f64::min);
        let retry = future
            .map(|r| format!(" retry_at={}", r as i64))
            .unwrap_or_default();
        chain.push(json!(format!("slot=exhausted queue{retry}")));
        return queue_decision(chain);
    }
    chain.push(json!(format!("slot=exhausted {on_exhausted}")));
    exhausted_decision(chain)
}

#[allow(clippy::too_many_arguments)]
fn pick(
    chain: &mut Vec<Value>,
    rows: &Map<String, Value>,
    fields_by_rung: &Map<String, Value>,
    capacity: &Value,
    index: usize,
    rung: &str,
    row_name: &str,
    state: &str,
    window: &str,
    note: &str,
    strict: Option<&(String, String)>,
) -> Value {
    let mut line = format!("slot {} capacity={state}", lane_label(rung, row_name),);
    if !window.is_empty() {
        line.push_str(&format!(" window={window}"));
    }
    if !note.is_empty() {
        line.push_str(&format!(" ({note})"));
    }
    chain.push(json!(line));
    let row = rows.get(row_name).cloned().unwrap_or(json!({}));
    let harness = row_value(&row, "harness");
    let model = row_value(&row, "model");
    let effort = row_value(&row, "effort");
    let route = row_value(&row, "route");
    let account = row_value(&row, "account");
    let mut lane_fields = Map::new();
    if let Some(inline) = fields_by_rung.get(rung).and_then(Value::as_object) {
        lane_fields = inline.clone();
    } else {
        for (k, v) in [
            ("provider", &harness),
            ("model", &model),
            ("effort", &effort),
            ("route", &route),
            ("account", &account),
        ] {
            if !v.is_empty() {
                lane_fields.insert(k.to_string(), Value::String(v.clone()));
            }
        }
    }
    let mut candidate = Map::new();
    candidate.insert("harness".into(), json!(harness));
    candidate.insert("model".into(), json!(model));
    candidate.insert("lane".into(), json!(row_name));
    candidate.insert("lane_rung".into(), json!(rung));
    candidate.insert("lane_index".into(), json!(index));
    candidate.insert("lane_fields".into(), Value::Object(lane_fields));
    if let Some((access, slot_v)) = strict {
        candidate.insert(
            "policy".into(),
            json!({
                "source": "config routing.enforce_inventory",
                "work_kind": slot_v,
                "operator_access": access,
            }),
        );
    }
    if !effort.is_empty() {
        candidate.insert("effort".into(), json!(effort));
    }
    // AC6-COORDINATE: the candidate carries the evidence that selected it.
    let mut evidence = Map::new();
    evidence.insert("capacity".into(), json!(state));
    evidence.insert("window".into(), json!(window));
    if !account.is_empty() && route.is_empty() {
        let ident = capacity
            .get(&harness)
            .and_then(|d| d.get("evidence"))
            .and_then(|e| e.get(&account))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        evidence.insert("identity".into(), json!(ident));
    }
    candidate.insert("evidence".into(), Value::Object(evidence));
    json!({
        "status": "pick",
        "candidate": Value::Object(candidate),
        "chain": chain,
    })
}

fn none(chain: Vec<Value>) -> Value {
    json!({"status": "none", "candidate": Value::Null, "chain": chain})
}

/// Capacity terminals keep the receipt vocabulary verbatim while naming their
/// kind for machine consumers.
fn queue_decision(chain: Vec<Value>) -> Value {
    json!({"status": "none", "candidate": Value::Null, "reason_kind": "capacity-queue", "chain": chain})
}

fn exhausted_decision(chain: Vec<Value>) -> Value {
    json!({"status": "none", "candidate": Value::Null, "reason_kind": "capacity-exhausted", "chain": chain})
}

/// A strict-policy refusal: the decision path is named, the candidate is
/// Null, and `reason_kind` tells machine consumers this apart from a
/// capacity queue or an unarmed legacy no-candidate.
fn refused_decision(chain: Vec<Value>, kind: &str, reason: &str) -> Value {
    json!({
        "status": "none",
        "candidate": Value::Null,
        "reason_kind": "policy-refusal",
        "refusal": kind,
        "reason": reason,
        "chain": chain,
    })
}

/// The native operator view a harness can show, if the harness has one on
/// this machine. Any other harness has no native view kind, so its rows can
/// never qualify while the operator is remote or unknown.
fn native_view_for(harness: &str) -> Option<&'static str> {
    match harness {
        "claude" => Some("claude-native"),
        "codex" => Some("codex-native"),
        _ => None,
    }
}

/// Rust owns the work-kind ruling: a planless target
/// performs planning and qualifies against the blueprint slot, while the
/// command stays target. A planned target, think, blueprint, review, crown
/// and every ops stage qualify against their own slots.
fn effective_work_kind(work_verb: &str, plan_present: bool) -> (String, Option<String>) {
    let verb = work_verb.trim().to_lowercase();
    match verb.as_str() {
        "target" if !plan_present => (
            "blueprint".to_string(),
            Some(format!(
                "slot note agents.profiles.target planless target -> blueprint eligibility (command stays target)"
            )),
        ),
        other => (other.to_string(), None),
    }
}

/// Print stdout/stderr and return the exit code. Used by `bin/client.rs`.
pub fn run_route_slot(args: &[String]) -> i32 {
    let (code, stdout, stderr) = run_route_slot_capture(args);
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    code
}

/// Test-friendly variant: returns (exit_code, stdout, stderr) without printing.
pub fn run_route_slot_capture(args: &[String]) -> (i32, String, String) {
    if args.first().map(String::as_str) == Some("audit") {
        return run_route_slot_audit(&args[1..]);
    }
    let payload: Value = if let Some(path) = args.first() {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        2,
                        String::new(),
                        format!("route-slot: bad payload file {path}: {e}\n"),
                    )
                }
            },
            Err(e) => {
                return (
                    2,
                    String::new(),
                    format!("route-slot: cannot read payload {path}: {e}\n"),
                )
            }
        }
    } else {
        let mut buf = String::new();
        match std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
            Ok(_) => match serde_json::from_str(&buf) {
                Ok(v) => v,
                Err(e) => return (2, String::new(), format!("route-slot: bad payload: {e}\n")),
            },
            Err(e) => {
                return (
                    2,
                    String::new(),
                    format!("route-slot: stdin read failed: {e}\n"),
                )
            }
        }
    };
    let out = resolve_slot_payload(&payload);
    (0, format!("{out}\n"), String::new())
}

/// `route-slot audit`: read-only completion evidence for the routing policy
/// (x-90a9). The snapshot loader is Python (`fno config route
/// audit-snapshot`), which reads config, registry, journal and decision
/// records through their established readers; the VERDICT is made here, the
/// same owner that qualifies every launch, and repeats read exactly.
///
/// Exit 0 prints ROUTING_POLICY_VERIFIED and names each verified session.
/// Anything missing, stale, contradictory or merely simulated exits 1 and
/// names the boundary that stopped it.
pub fn run_route_slot_audit(args: &[String]) -> (i32, String, String) {
    let mut project = String::new();
    let mut node = String::new();
    let mut since = String::from("30m");
    let mut json_out = false;
    let mut snapshot_path: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--project" => project = it.next().cloned().unwrap_or_default(),
            "--node" => node = it.next().cloned().unwrap_or_default(),
            "--since" => since = it.next().cloned().unwrap_or_else(|| "30m".into()),
            "--json" => json_out = true,
            "--snapshot" => snapshot_path = it.next().map(String::as_str),
            other => {
                return (
                    2,
                    String::new(),
                    format!("route-slot audit: unknown argument {other:?}\n"),
                )
            }
        }
    }
    let snapshot_text = match snapshot_path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                return (
                    1,
                    String::new(),
                    format!("route-slot audit: cannot read snapshot {path}: {e}\n"),
                )
            }
        },
        None => {
            // No snapshot handed in: load it natively. Config facts ride the
            // public inventory surface (the fingerprint algorithm belongs to
            // the Python config reader); sessions come from the machine
            // stores this binary already owns.
            match load_audit_snapshot(&project, &node, &since) {
                Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                Err(e) => return (1, String::new(), format!("route-slot audit: {e}\n")),
            }
        }
    };
    let snapshot: Value = match serde_json::from_str(&snapshot_text) {
        Ok(v) => v,
        Err(e) => {
            return (
                1,
                String::new(),
                format!("route-slot audit: bad snapshot: {e}\n"),
            )
        }
    };
    let (report, code) = audit_verify(&snapshot);
    if json_out {
        (code, format!("{report}\n"), String::new())
    } else {
        let verdict = report["verdict"].as_str().unwrap_or("UNKNOWN");
        let mut text = String::new();
        if code == 0 {
            text.push_str("ROUTING_POLICY_VERIFIED\n");
        }
        text.push_str(&format!("verdict: {verdict}\n"));
        if let Some(sessions) = report["sessions"].as_array() {
            for s in sessions {
                text.push_str(&format!(
                    "  session {} {} account={} model={:?} basis={:?}\n",
                    s["session_id"].as_str().unwrap_or("-"),
                    s["harness"].as_str().unwrap_or("-"),
                    s["account"].as_str().unwrap_or("-"),
                    s["model"].as_str().unwrap_or(""),
                    s["model_basis"].as_str().unwrap_or(""),
                ));
            }
        }
        if let Some(b) = report["boundaries"].as_array() {
            for line in b {
                text.push_str(&format!(
                    "  boundary {}: {}\n",
                    line["boundary"].as_str().unwrap_or("-"),
                    line["detail"].as_str().unwrap_or(""),
                ));
            }
        }
        (code, text, String::new())
    }
}

/// The pure verdict over one bounded snapshot. No I/O: the same snapshot in,
/// the same verdict out. Every boundary names the missing evidence class from
/// the plan, so a negative audit is diagnostic, not just non-zero.
pub fn audit_verify(snapshot: &Value) -> (Value, i32) {
    let mut boundaries: Vec<Value> = Vec::new();
    let policy = snapshot.get("policy").cloned().unwrap_or(json!({}));
    let enforced = policy
        .get("enforce_inventory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fingerprint = snapshot
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !enforced {
        boundaries.push(json!({
            "boundary": "routing-not-enforced",
            "detail": "config routing.enforce_inventory is off; no armed policy to verify",
        }));
    }
    let sessions = snapshot
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sessions.is_empty() {
        boundaries.push(json!({
            "boundary": "no-launch",
            "detail": "no session for this node inside the window; a preview alone verifies nothing",
        }));
    }
    let mut verified: Vec<Value> = Vec::new();
    for s in &sessions {
        let sid = s.get("session_id").and_then(Value::as_str).unwrap_or("");
        if sid.is_empty() {
            boundaries.push(json!({
                "boundary": "missing-session-id",
                "detail": s.get("name").and_then(Value::as_str).unwrap_or("?"),
            }));
            continue;
        }
        let account = s.get("account").and_then(Value::as_str).unwrap_or("");
        if account.is_empty() {
            boundaries.push(json!({
                "boundary": "missing-account-identity",
                "detail": format!("session {sid} names no account record"),
            }));
            continue;
        }
        let receipt_fp = s
            .get("receipt_fingerprint")
            .and_then(Value::as_str)
            .unwrap_or("");
        if receipt_fp.is_empty() {
            boundaries.push(json!({
                "boundary": "no-receipt",
                "detail": format!("session {sid} has no spawn decision receipt in the journal"),
            }));
            continue;
        }
        if receipt_fp != fingerprint {
            boundaries.push(json!({
                "boundary": "stale-receipt",
                "detail": format!(
                    "session {sid} launched on fingerprint {receipt_fp}, config is now {fingerprint}"
                ),
            }));
            continue;
        }
        let model = s.get("model").and_then(Value::as_str).unwrap_or("");
        let basis = s.get("model_basis").and_then(Value::as_str).unwrap_or("");
        if model.is_empty() || basis != "verified" {
            boundaries.push(json!({
                "boundary": "unobserved-requested-model",
                "detail": format!(
                    "session {sid} model {model:?} carries basis {basis:?}; a requested label is not an observation"
                ),
            }));
            continue;
        }
        match view_evidence(s, sid, &fingerprint) {
            Ok(()) => verified.push(json!({
                "session_id": sid,
                "harness": s.get("harness").and_then(Value::as_str).unwrap_or(""),
                "account": account,
                "model": model,
                "model_basis": basis,
            })),
            Err(boundary) => boundaries.push(boundary),
        }
    }
    if verified.is_empty() && !boundaries.is_empty() {
        let report = json!({"verdict": "ROUTING_POLICY_INCOMPLETE", "boundaries": boundaries});
        return (report, 1);
    }
    if boundaries.is_empty() {
        let report = json!({"verdict": "ROUTING_POLICY_VERIFIED", "sessions": verified});
        return (report, 0);
    }
    // Some sessions verified, some did not: the incomplete ones keep the
    // audit from a clean pass, and their boundaries are the answer.
    let report = json!({
        "verdict": "ROUTING_POLICY_INCOMPLETE",
        "sessions": verified,
        "boundaries": boundaries,
    });
    (report, 1)
}

/// The operator-view record for one session: an existing live decision under
/// subject `routing-view:<session-id>` whose decision text is JSON carrying
/// the view, the config fingerprint it was confirmed under, and the session
/// it names. A worker or peer assertion is not operator confirmation; this
/// record exists only because the operator recorded it.
fn view_evidence(session: &Value, sid: &str, fingerprint: &str) -> Result<(), Value> {
    let records = session
        .get("view_records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let wanted_subject = format!("routing-view:{sid}");
    let record = records
        .iter()
        .find(|r| r.get("subject").and_then(Value::as_str) == Some(wanted_subject.as_str()));
    let record = match record {
        None => {
            return Err(json!({
                "boundary": "missing-operator-view",
                "detail": format!(
                    "no decision record under routing-view:{sid}; \
                     the operator must confirm that exact session in the named view"
                ),
            }))
        }
        Some(r) => r,
    };
    let lifecycle = record
        .get("lifecycle")
        .and_then(Value::as_str)
        .unwrap_or("");
    if lifecycle != "live" {
        return Err(json!({
            "boundary": "view-record-not-live",
            "detail": format!("routing-view:{sid} is {lifecycle:?}; a retracted or superseded confirmation verifies nothing"),
        }));
    }
    let text = record.get("decision").and_then(Value::as_str).unwrap_or("");
    let body: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return Err(json!({
                "boundary": "view-record-unparseable",
                "detail": format!("routing-view:{sid} decision text is not the JSON evidence shape"),
            }))
        }
    };
    let view = body.get("view").and_then(Value::as_str).unwrap_or("");
    if view != "claude-native" && view != "codex-native" {
        return Err(json!({
            "boundary": "view-record-bad-view",
            "detail": format!("routing-view:{sid} names view {view:?}; expected claude-native or codex-native"),
        }));
    }
    if body.get("fingerprint").and_then(Value::as_str) != Some(fingerprint) {
        return Err(json!({
            "boundary": "view-fingerprint-mismatch",
            "detail": format!(
                "routing-view:{sid} was confirmed under a different configuration fingerprint than the current one"
            ),
        }));
    }
    let recorded_sid = body.get("session_id").and_then(Value::as_str).unwrap_or("");
    if recorded_sid != sid {
        return Err(json!({
            "boundary": "view-session-mismatch",
            "detail": format!("routing-view record names session {recorded_sid:?}, not {sid:?}"),
        }));
    }
    Ok(())
}

/// Config facts (fingerprint, policy) come from the public inventory surface;
/// everything else loads from the machine stores. A missing front door is an
/// incomplete terminal, never a pass.
fn load_audit_snapshot(project: &str, node: &str, since: &str) -> Result<Value, String> {
    let inv = std::process::Command::new("fno")
        .args(["config", "route", "inventory", "--json"])
        .output()
        .map_err(|e| format!("no fno front door for config facts: {e}"))?;
    if !inv.status.success() {
        return Err("inventory read failed for config facts".to_string());
    }
    let facts: Value =
        serde_json::from_slice(&inv.stdout).map_err(|e| format!("bad inventory json: {e}"))?;
    let since_seconds = parse_since_seconds(since)?;
    let home = crate::paths::AgentsHome::from_env();
    let state_root = home
        .root()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.root().to_path_buf());
    audit_load_snapshot(
        &facts,
        &state_root,
        &home.registry_json(),
        project,
        node,
        since_seconds,
    )
}

/// The evidence window: `30m`, `2h`, `7d`, or bare seconds.
fn parse_since_seconds(text: &str) -> Result<i64, String> {
    let text = text.trim();
    let units: [(&str, i64); 4] = [("d", 86400), ("h", 3600), ("m", 60), ("s", 1)];
    for (suffix, seconds) in units {
        if let Some(value) = text
            .strip_suffix(suffix)
            .and_then(|v| v.parse::<i64>().ok())
        {
            if text.len() > suffix.len() {
                return Ok(value * seconds);
            }
        }
    }
    text.parse::<i64>()
        .map_err(|_| format!("--since must look like 30m, 2h or 7d: {text:?}"))
}

/// The native snapshot loader. Config facts arrive in `config_facts` (the
/// caller execs the public inventory surface for them: the fingerprint's
/// algorithm belongs to the Python config reader, and duplicating it here
/// would fork the one answer the receipts carry). Sessions, receipts and
/// view records load from the machine stores this binary already reads.
///
/// Pure over its inputs: the same files and facts in, the same snapshot out.
pub(crate) fn audit_load_snapshot(
    config_facts: &Value,
    state_root: &Path,
    registry_path: &Path,
    project: &str,
    node: &str,
    since_seconds: i64,
) -> Result<Value, String> {
    use std::collections::BTreeMap;

    let fingerprint = config_facts
        .get("fingerprint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let policy = config_facts.get("policy").cloned().unwrap_or(json!({}));
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(since_seconds);
    let within = |ts: &str| -> bool {
        chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| t.with_timezone(&chrono::Utc) >= cutoff)
            .unwrap_or(false)
    };

    // Spawn decision receipts: the newest fingerprint per spawn name inside
    // the window.
    let mut receipts: BTreeMap<String, String> = BTreeMap::new();
    match std::fs::read_to_string(state_root.join("events.jsonl")) {
        Ok(text) => {
            for line in text.lines() {
                let Ok(row) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if row.get("kind").and_then(Value::as_str) != Some("spawn_defaults_applied") {
                    continue;
                }
                if !within(row.get("ts").and_then(Value::as_str).unwrap_or("")) {
                    continue;
                }
                let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                let fp = row.get("fingerprint").and_then(Value::as_str).unwrap_or("");
                if !name.is_empty() && !fp.is_empty() {
                    receipts.insert(name.to_string(), fp.to_string());
                }
            }
        }
        Err(_) => {} // no journal: no receipts, the verifier names the boundary
    }

    // View records: one row per subject prefix routing-view:. Retirement
    // (retraction or a superseding ruling) is resolved in a second pass over
    // the collected rows, so row order can never leave a withdrawn
    // confirmation reading as live. The Python decisions reader stays the
    // format owner; this is a consumer-side consistency read.
    let mut view_rows: BTreeMap<String, (String, String, String)> = BTreeMap::new(); // subject -> (decision, ts, decision_id)
    let mut retired: BTreeMap<String, ()> = BTreeMap::new();
    let mut candidates: Vec<(String, String, String, String)> = Vec::new(); // subject, decision, ts, decision_id
    match std::fs::read_to_string(state_root.join("decisions.jsonl")) {
        Ok(text) => {
            for line in text.lines() {
                let Ok(row) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
                let data = row.get("data").cloned().unwrap_or(json!({}));
                if kind == "decision_retracted" {
                    if let Some(target) = data.get("target_decision_id").and_then(Value::as_str) {
                        if !target.is_empty() {
                            retired.insert(target.to_string(), ());
                        }
                    }
                    continue;
                }
                let subject = data.get("subject").and_then(Value::as_str).unwrap_or("");
                if !subject.starts_with("routing-view:") {
                    continue;
                }
                // A ruling naming another in `supersedes` retires it, the
                // same derivation the Python decisions reader applies.
                if let Some(superseded) = data.get("supersedes").and_then(Value::as_str) {
                    if !superseded.is_empty() {
                        retired.insert(superseded.to_string(), ());
                    }
                }
                candidates.push((
                    subject.to_string(),
                    data.get("decision")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    row.get("ts")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    data.get("decision_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ));
            }
        }
        Err(_) => {} // no index: no view records, the verifier names the boundary
    }
    for (subject, decision, ts, did) in candidates {
        if retired.contains_key(did.as_str()) {
            continue;
        }
        view_rows.insert(subject, (decision, ts, did));
    }

    // Sessions: registry rows naming the node inside the window.
    let mut sessions: Vec<Value> = Vec::new();
    if node.is_empty() {
        return Ok(json!({}));
    }
    match crate::state::load_registry(registry_path) {
        Ok(registry) => {
            for entry in &registry.entries {
                if entry.node.as_deref() != Some(node) {
                    continue;
                }
                let root = if entry.project_root.is_empty() {
                    entry.cwd.as_str()
                } else {
                    entry.project_root.as_str()
                };
                if !project.is_empty() && !root.starts_with(project) {
                    continue;
                }
                if !within(&entry.created_at) {
                    continue;
                }
                let sid = entry
                    .harness_session_id
                    .clone()
                    .or_else(|| entry.fno_id.clone())
                    .unwrap_or_default();
                let name = entry.name.clone();
                let view_records: Vec<Value> = view_rows
                    .iter()
                    .filter(|(subject, _)| subject.as_str() == &format!("routing-view:{sid}"))
                    .filter(|(_, (decision, _, did))| {
                        // A retracted or superseded confirmation verifies
                        // nothing; the Python reader owns the full lifecycle.
                        !retired.contains_key(did.as_str()) && !decision.is_empty()
                    })
                    .map(|(subject, (decision, ts, did))| {
                        json!({
                            "subject": subject,
                            "decision": decision,
                            "ts": ts,
                            "decision_id": did.to_string(),
                            "lifecycle": "live",
                        })
                    })
                    .collect();
                sessions.push(json!({
                    "session_id": sid,
                    "name": name,
                    "harness": entry.harness.clone(),
                    "model": entry.model.clone().unwrap_or_default(),
                    "model_basis": entry.model_basis.clone().unwrap_or_default(),
                    "requested_model": entry.requested_model.clone().unwrap_or_default(),
                    "account": entry.account_record_id.clone().unwrap_or_default(),
                    "created_at": entry.created_at,
                    "receipt_fingerprint": receipts.get(&name).cloned().unwrap_or_default(),
                    "view_records": view_records,
                }));
            }
        }
        Err(e) => return Err(format!("registry unreadable: {e}")),
    }
    Ok(json!({
        "project": project,
        "node": node,
        "fingerprint": fingerprint,
        "policy": policy,
        "sessions": sessions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(overrides: Value) -> Value {
        let mut base = json!({
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["flash-x", "sonnet-x"],
            "declared_rows": {
                "flash-x": {"name": "flash-x", "harness": "claude", "model": "glm",
                            "band": "low", "account": "zai-main", "route": "zai/glm"},
                "sonnet-x": {"name": "sonnet-x", "harness": "claude", "model": "sonnet"},
            },
            "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy",
                        "on_unknown": "allow", "by_difficulty": {}},
            "node": null,
            "capacity": {"claude": {"state": "ok", "window": "w",
                                    "accounts": {"zai-main": "ok"},
                                    "evidence": {}, "resets": {}}},
            "vendor_counts": {}, "vendor_caps": {}, "vendor_count_errors": {},
            "thread_seatable": {}, "substrate": null, "permission_mode": null,
            "constrain_harness": null,
            "explicit_lane": false, "explicit_model": false, "gate_bypassed": false,
        });
        if let (Some(base_obj), Some(ovr)) = (base.as_object_mut(), overrides.as_object()) {
            for (k, v) in ovr {
                base_obj.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn chain_of(out: &Value) -> Vec<String> {
        out["chain"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn grid_picks_the_first_clearing_candidate() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [], "node": {"difficulty": "high", "priority": "p2"},
            "model_occupied": false,
            "inventory": {"declared": true, "objective": "cheapest-that-clears",
                          "prefer_harness": "", "rows": [
                {"name": "flash", "harness": "claude", "model": "glm", "band": "low"},
                {"name": "sonnet", "harness": "claude", "model": "sonnet", "band": "high"},
            ]},
        })));
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["model"], "sonnet");
        let chain = chain_of(&out);
        assert_eq!(chain[1], "grid difficulty(high) priority(p2)");
        assert!(chain
            .iter()
            .any(|l| l == "grid candidate claude/sonnet capacity=ok window=w"));
    }

    #[test]
    fn grid_candidate_carries_the_rows_route_and_account() {
        // AC2-HP: a routed row's candidate carries route and account, the
        // fields the lane leg always emitted and the grid leg dropped.
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [], "node": {"difficulty": "high", "priority": "p2"},
            "inventory": {"declared": true, "objective": "cheapest-that-clears",
                          "rows": [
                {"name": "flash", "harness": "claude", "model": "glm", "band": "high",
                 "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
                {"name": "bare", "harness": "claude", "model": "sonnet", "band": "low"},
            ]},
        })));
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["harness"], "claude");
        assert_eq!(out["candidate"]["model"], "glm");
        assert_eq!(out["candidate"]["route"], "zai/glm-5.3-flash[1m]");
        assert_eq!(out["candidate"]["account"], "zai-main");
        // AC2/AC4-EDGE: a routeless winner emits neither key, byte-identical
        // to the pre-port shape for every anthropic row.
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [], "node": {"difficulty": "high", "priority": "p3"},
            "inventory": {"declared": true, "objective": "cheapest-that-clears",
                          "rows": [
                {"name": "flash", "harness": "claude", "model": "glm", "band": "high",
                 "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
                {"name": "bare", "harness": "claude", "model": "sonnet", "band": "low"},
            ]},
        })));
        assert_eq!(out["candidate"]["model"], "sonnet");
        assert!(out["candidate"].get("route").is_none());
        assert!(out["candidate"].get("account").is_none());
    }

    #[test]
    fn grid_refuses_undeclared_inventory_and_bad_priority() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [], "node": {"difficulty": "high", "priority": "p2"},
            "inventory": {"declared": false, "rows": []},
        })));
        assert!(chain_of(&out).contains(&"grid=no-inventory-declared".to_string()));
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [],
            "node": {"difficulty": "high", "priority": "p9"},
            "inventory": {"declared": true, "rows": [
                {"name": "r", "harness": "claude", "model": "m", "band": "high"}]},
        })));
        assert!(chain_of(&out).contains(&"grid=invalid-input".to_string()));
    }

    #[test]
    fn grid_p3_prefers_the_low_band_and_unbanded_ranks_last() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [], "node": {"difficulty": "high", "priority": "p3"},
            "inventory": {"declared": true, "objective": "cheapest-that-clears",
                          "rows": [
                {"name": "unb", "harness": "claude", "model": "m-unb"},
                {"name": "lowrow", "harness": "claude", "model": "m-low", "band": "low"},
            ]},
        })));
        assert_eq!(out["candidate"]["model"], "m-low");
        // The unbanded row was walked first in declared order and skipped on
        // the band, so its receipt never prints; the picked low row prints.
        let chain = chain_of(&out);
        assert!(chain
            .iter()
            .any(|l| l.contains("grid candidate claude/lowrow capacity=ok window=w")));
    }

    #[test]
    fn grid_exhausted_candidates_are_skipped_and_the_terminal_names_it() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [], "node": {"difficulty": "low", "priority": "p2"},
            "capacity": {"claude": {"state": "exhausted", "window": "lock",
                                    "accounts": {}, "evidence": {}, "resets": {}}},
            "inventory": {"declared": true, "rows": [
                {"name": "r", "harness": "claude", "model": "m", "band": "low"}]},
        })));
        assert_eq!(out["status"], "none");
        assert_eq!(
            chain_of(&out).last().unwrap(),
            "grid=no-available-candidate"
        );
    }

    #[test]
    fn tier_resolves_degrades_and_falls_through() {
        let inv = json!({"rows": [
            {"name": "lowrow", "harness": "claude", "model": "m-low", "band": "low"},
            {"name": "highrow", "harness": "claude", "model": "m-high", "band": "high"},
        ]});
        let out = resolve_slot_payload(&json!({"mode": "tier", "tier": "high", "inventory": inv}));
        assert_eq!(out["model"], "m-high");
        let out = resolve_slot_payload(&json!({"mode": "tier", "tier": "max", "inventory": inv}));
        assert_eq!(out["model"], "m-high");
        assert!(chain_of(&out)
            .iter()
            .any(|l| l.contains("empty -> degrade -> highrow")));
        let out =
            resolve_slot_payload(&json!({"mode": "tier", "tier": "banana", "inventory": inv}));
        assert_eq!(out["model"], Value::Null);
        assert!(chain_of(&out).contains(&"unknown-tier -> provider default".to_string()));
    }

    #[test]
    fn states_readout_lists_every_lane() {
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["flash-x", "ghost-x"],
            "declared_rows": {"flash-x": {"name": "flash-x", "harness": "claude",
                                          "model": "glm", "account": "zai-main"}},
            "capacity": {"claude": {"state": "ok", "window": "w",
                                    "accounts": {"zai-main": "exhausted"},
                                    "evidence": {}, "resets": {}}},
        }));
        assert_eq!(out["status"], "states");
        let states = out["lane_states"].as_array().unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(states[0]["state"], "exhausted");
        assert_eq!(states[1]["state"], "no-such-row");
    }

    #[test]
    fn states_verdict_names_the_lane_a_spawn_would_take() {
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["flash-x"],
            "declared_rows": {"flash-x": {"name": "flash-x", "harness": "claude",
                                          "model": "glm"}},
            "capacity": {"claude": {"state": "ok"}},
            "profile": {"on_exhausted": "refuse"},
        }));
        assert_eq!(out["routing"], "armed");
        assert_eq!(out["would_take"], "agents.profiles.target.lanes[0] flash-x");
        assert_eq!(out["on_exhausted"], "refuse");
        assert_eq!(out["on_low"], "prefer_healthy");
        assert_eq!(out["on_unknown"], "allow");
    }

    #[test]
    fn states_all_lanes_exhausted_reads_capacity_held_with_the_terminal() {
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["flash-x"],
            "declared_rows": {"flash-x": {"name": "flash-x", "harness": "claude",
                                          "model": "glm", "account": "zai-main"}},
            "capacity": {"claude": {"state": "exhausted",
                                    "accounts": {"zai-main": "exhausted"}}},
            "profile": {"on_exhausted": "queue"},
        }));
        assert_eq!(out["routing"], "capacity-held");
        assert!(out["would_take"].as_str().unwrap().contains("exhausted"));
    }

    #[test]
    fn states_policy_refusal_reads_policy_held_with_the_reasons() {
        // A remote operator with only unverified views: every lane is skipped
        // by the operator_access filter. The readout must say policy-held,
        // never "exhausted", and carry the per-lane skip reasons.
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["glm-x"],
            "declared_rows": {"glm-x": {"name": "glm-x", "harness": "claude",
                                        "model": "glm", "route": "zai,glm"}},
            "slot_by_verb": {"blueprint": {"rung_base": "agents.profiles.blueprint",
                                           "lanes_raw": ["glm-x"]}},
            "policy": {"enforce_inventory": true, "operator_access": "remote"},
            "capacity": {"claude": {"state": "ok", "accounts": {}}},
        }));
        assert_eq!(out["routing"], "policy-held");
        assert_eq!(out["operator_access"], "remote");
        assert_eq!(out["policy_source"], "config routing.enforce_inventory");
        let skipped = out["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0]
            .as_str()
            .unwrap()
            .contains("operator_access=remote"));
    }

    #[test]
    fn states_no_lanes_names_the_grid_fallthrough_and_omits_policies() {
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": [],
            "inventory": {"declared": true,
                          "rows": [{"name": "a", "harness": "claude", "model": "m"},
                                   {"name": "b", "harness": "codex", "model": "m"}]},
        }));
        assert_eq!(out["would_take"], "no lanes; grid over 2 rows");
        assert_eq!(out["routing"], "unarmed");
        assert_eq!(out["lane_states"].as_array().unwrap().len(), 0);
        assert!(out.get("on_exhausted").is_none());
        let out = resolve_slot_payload(&json!({
            "mode": "states", "rung_base": "agents.profiles.target",
            "lanes_raw": [], "inventory": {"declared": false, "rows": []},
        }));
        assert_eq!(out["would_take"], "no lanes; no inventory; harness default");
    }

    #[test]
    fn states_invalid_policy_is_displayed_with_the_invalid_marker() {
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["flash-x"],
            "declared_rows": {"flash-x": {"name": "flash-x", "harness": "claude",
                                          "model": "glm"}},
            "capacity": {"claude": {"state": "ok"}},
            "profile": {"on_exhausted": "Bogus"},
        }));
        assert_eq!(out["on_exhausted"], "Bogus (invalid)");
        // The walk refuses the same config, so the verdict names the fault.
        assert!(out["would_take"].as_str().unwrap().contains("not one of"));
        assert_eq!(out["routing"], "unarmed");
    }

    #[test]
    fn states_fault_paths_still_display_the_policy_lines() {
        // A malformed overlay lanes list refuses, and the readout keeps the
        // policy display it always showed for a peeked-non-empty slot.
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": [],
            "profile": {"on_exhausted": "queue", "by_difficulty":
                        {"high": {"lanes": "garbage"}}},
        }));
        assert_eq!(out["on_exhausted"], "queue");
        assert_eq!(out["on_low"], "prefer_healthy");
        assert_eq!(out["on_unknown"], "allow");
        assert!(out["would_take"]
            .as_str()
            .unwrap()
            .contains("must be a non-empty list"));
        // A fold fault (lane names nothing declared) keeps them too.
        let out = resolve_slot_payload(&json!({
            "mode": "states",
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["ghost-x"],
            "declared_rows": {},
            "profile": {"on_exhausted": "degrade"},
        }));
        assert_eq!(out["on_exhausted"], "degrade");
        assert_eq!(out["routing"], "unarmed");
    }

    #[test]
    fn string_lane_names_a_declared_row() {
        let out = resolve_slot_payload(&payload(json!({})));
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["model"], "glm");
        assert_eq!(
            chain_of(&out)[1],
            "slot agents.profiles.target.lanes[0] flash-x capacity=ok window=w"
        );
    }

    #[test]
    fn out_of_enum_on_low_refuses_instead_of_coercing() {
        let out = resolve_slot_payload(&payload(json!({
            "profile": {"on_exhausted": "refuse", "on_low": "maybe",
                        "on_unknown": "allow", "by_difficulty": {}},
        })));
        assert_eq!(out["status"], "none");
        let chain = chain_of(&out);
        assert!(chain
            .last()
            .unwrap()
            .starts_with("slot=config agents.profiles.target.on_low"));
    }

    #[test]
    fn inline_lanes_print_the_rung_once() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": [{"provider": "claude", "model": "m-2"}],
        })));
        assert_eq!(out["status"], "pick");
        let chain = chain_of(&out);
        assert!(
            chain[1].starts_with("slot agents.profiles.target.lanes[0] ")
                && !chain[1].contains("lanes[0] agents.profiles")
        );
    }

    #[test]
    fn harness_pin_skips_foreign_lanes() {
        let out = resolve_slot_payload(&payload(json!({
            "constrain_harness": "codex",
        })));
        assert_eq!(out["status"], "none");
        let chain = chain_of(&out);
        assert!(chain
            .iter()
            .any(|l| l.contains("is not the pinned \"codex\"")));
    }

    #[test]
    fn malformed_by_difficulty_refuses() {
        let out = resolve_slot_payload(&payload(json!({
            "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy",
                        "on_unknown": "allow",
                        "by_difficulty": {"urgent": {"lanes": ["flash-x"]}}},
        })));
        assert_eq!(out["status"], "none");
        assert!(chain_of(&out)[0].contains("is not low|medium|high"));

        let out = resolve_slot_payload(&payload(json!({
            "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy",
                        "on_unknown": "allow",
                        "by_difficulty": {"high": {"lanes": [], "bogus": "x"}}},
        })));
        assert_eq!(out["status"], "none");
        assert!(chain_of(&out)[0].contains("has unknown field"));
    }

    #[test]
    fn non_iterable_lanes_is_a_config_fault() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": "flash-x",
        })));
        assert_eq!(out["status"], "none");
        assert!(chain_of(&out)[0].contains("must be a list"));
    }

    #[test]
    fn queue_terminal_carries_retry_at() {
        let out = resolve_slot_payload(&payload(json!({
            "profile": {"on_exhausted": "queue", "on_low": "prefer_healthy",
                        "on_unknown": "allow", "by_difficulty": {}},
            "capacity": {"claude": {"state": "exhausted", "window": "lock",
                                    "accounts": {"zai-main": "exhausted"},
                                    "evidence": {},
                                    "resets": {"zai-main": 1900000000.0}}},
            "gate_bypassed": false,
        })));
        assert_eq!(out["status"], "none");
        let chain = chain_of(&out);
        assert!(chain
            .last()
            .unwrap()
            .starts_with("slot=exhausted queue retry_at=1900000000"));
    }

    #[test]
    fn identity_only_exhaustion_names_the_manual_terminal() {
        let out = resolve_slot_payload(&payload(json!({
            "lanes_raw": ["alt-a", "alt-b"],
            "declared_rows": {
                "alt-a": {"name": "alt-a", "harness": "claude", "model": "a",
                          "account": "readyrule"},
                "alt-b": {"name": "alt-b", "harness": "claude", "model": "b",
                          "account": "ghost"},
            },
            "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy",
                        "on_unknown": "skip", "by_difficulty": {}},
            "capacity": {"claude": {"state": "ok", "window": "w",
                                    "accounts": {}, "evidence": {"makers": "proven"},
                                    "resets": {}}},
        })));
        assert_eq!(out["status"], "none");
        let chain = chain_of(&out);
        assert_eq!(chain.last().unwrap(), "slot=manual_account_switch_required");
    }

    #[test]
    fn prefer_healthy_demotes_low_then_takes_it_when_all_low() {
        let healthy = payload(json!({
            "capacity": {"claude": {"state": "low", "window": "w",
                                    "accounts": {"zai-main": "low"}, "evidence": {},
                                    "resets": {}}},
        }));
        // flash-x reads low (account), sonnet-x reads the harness aggregate low.
        let out = resolve_slot_payload(&healthy);
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["lane"], "flash-x");
        let chain = chain_of(&out);
        assert!(chain.iter().any(
            |l| l.contains("slot demote agents.profiles.target.lanes[0] flash-x capacity=low")
        ));
        assert!(chain
            .last()
            .unwrap()
            .contains("(no healthy lane; on_low=prefer_healthy)"));
    }

    #[test]
    fn vendor_cap_skip_uses_the_gathered_count() {
        let out = resolve_slot_payload(&payload(json!({
            "vendor_counts": {"zai": 2}, "vendor_caps": {"zai": 2},
        })));
        assert_eq!(out["status"], "pick");
        let chain = chain_of(&out);
        assert!(chain.iter().any(|l| l.contains("provider zai at 2 of 2")));
    }

    // -------------------------------------------------------------------
    // The strict inventory policy leg
    // -------------------------------------------------------------------

    fn strict_payload(overrides: Value) -> Value {
        let mut base = payload(json!({
            "policy": {"enforce_inventory": true, "operator_access": "unknown"},
            "work_verb": "target",
            "node": {"difficulty": "high", "priority": "p1", "plan_path": ""},
            "slot_by_verb": {
                "blueprint": {
                    "rung_base": "agents.profiles.blueprint",
                    "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                    "lanes_raw": ["opus-x"],
                },
                "target": {
                    "rung_base": "agents.profiles.target",
                    "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                    "lanes_raw": ["flash-x"],
                },
            },
        }));
        if let (Some(base_obj), Some(ovr)) = (base.as_object_mut(), overrides.as_object()) {
            for (k, v) in ovr {
                base_obj.insert(k.clone(), v.clone());
            }
        }
        base
    }

    #[test]
    fn strict_planless_target_rides_the_blueprint_slot_and_names_the_work_kind() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "declared_rows": {
                "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                           "operator_view": "claude-native"},
                "flash-x": {"name": "flash-x", "harness": "claude", "model": "glm",
                            "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
            },
            "capacity": {"claude": {"state": "ok", "window": "w",
                                    "accounts": {"zai-main": "ok"}, "evidence": {}, "resets": {}}},
        })));
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["model"], "claude-opus-5");
        assert_eq!(out["candidate"]["policy"]["work_kind"], "blueprint");
        assert_eq!(out["candidate"]["policy"]["operator_access"], "unknown");
        assert!(
            chain_of(&out)
                .iter()
                .any(|l| l
                    .contains("planless target -> blueprint eligibility (command stays target)"))
        );
    }

    #[test]
    fn strict_explicit_glm_on_blueprint_work_refuses_by_name() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "declared_rows": {
                "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                           "operator_view": "claude-native"},
            },
            "explicit_model_value": "glm",
        })));
        assert_eq!(out["status"], "none");
        assert_eq!(out["refusal"], "policy-coordinate-not-in-slot");
        assert!(chain_of(&out)
            .iter()
            .any(|l| l.contains("slot=strict-refusal explicit model \"glm\"")));
    }

    #[test]
    fn strict_remote_filter_skips_rows_without_a_verified_native_view() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "policy": {"enforce_inventory": true, "operator_access": "remote"},
            "node": {"difficulty": "high", "priority": "p1", "plan_path": "/plans/p.md"},
            "slot_by_verb": {
                "target": {
                    "rung_base": "agents.profiles.target",
                    "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                    "lanes_raw": ["flash-x", "opus-x"],
                },
            },
            "declared_rows": {
                "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                           "operator_view": "claude-native"},
                "flash-x": {"name": "flash-x", "harness": "claude", "model": "glm",
                            "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
            },
            "capacity": {"claude": {"state": "ok", "window": "w",
                                    "accounts": {"zai-main": "ok"}, "evidence": {}, "resets": {}}},
        })));
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["model"], "claude-opus-5");
        assert!(chain_of(&out)
            .iter()
            .any(|l| l.contains("no verified native view (operator_access=remote)")));
    }

    #[test]
    fn strict_local_admits_the_zai_lane() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "policy": {"enforce_inventory": true, "operator_access": "local"},
            "node": {"difficulty": "high", "priority": "p1", "plan_path": "/plans/p.md"},
            "slot_by_verb": {
                "target": {
                    "rung_base": "agents.profiles.target",
                    "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                    "lanes_raw": ["flash-x"],
                },
            },
            "declared_rows": {
                "flash-x": {"name": "flash-x", "harness": "claude", "model": "glm",
                            "route": "zai/glm-5.3-flash[1m]", "account": "zai-main"},
            },
            "capacity": {"claude": {"state": "ok", "window": "w",
                                    "accounts": {"zai-main": "ok"}, "evidence": {}, "resets": {}}},
        })));
        assert_eq!(out["status"], "pick");
        assert_eq!(out["candidate"]["model"], "glm");
    }

    #[test]
    fn strict_mislabeled_native_view_refuses() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "slot_by_verb": {
                "blueprint": {
                    "rung_base": "agents.profiles.blueprint",
                    "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                    "lanes_raw": ["bad-row"],
                },
            },
            "declared_rows": {
                "bad-row": {"name": "bad-row", "harness": "claude", "model": "glm",
                            "route": "zai/glm", "operator_view": "claude-native"},
            },
        })));
        assert_eq!(out["status"], "none");
        assert_eq!(out["refusal"], "policy-view-contradiction");
        assert!(chain_of(&out)
            .iter()
            .any(|l| l.contains("operator_view=\"claude-native\"")));
    }

    #[test]
    fn strict_no_declared_slot_refuses_rather_than_defaulting() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "slot_by_verb": {},
        })));
        assert_eq!(out["status"], "none");
        assert_eq!(out["refusal"], "policy-no-declared-slot");
        assert!(chain_of(&out)
            .iter()
            .any(|l| l.contains("strict routing refuses the harness default")));
    }

    #[test]
    fn strict_capacity_terminal_keeps_the_queue_vocabulary_and_kind() {
        let out = resolve_slot_payload(&strict_payload(json!({
            "declared_rows": {
                "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                           "operator_view": "claude-native"},
            },
            "capacity": {"claude": {"state": "exhausted", "window": "lock",
                                    "accounts": {}, "evidence": {},
                                    "resets": {"opus": 1900000000.0}}},
            "slot_by_verb": {
                "blueprint": {
                    "rung_base": "agents.profiles.blueprint",
                    "profile": {"on_exhausted": "queue", "on_low": "prefer_healthy", "on_unknown": "allow"},
                    "lanes_raw": ["opus-x"],
                },
            },
        })));
        assert_eq!(out["status"], "none");
        assert_eq!(out["reason_kind"], "capacity-queue");
        assert!(chain_of(&out)
            .iter()
            .any(|l| l.starts_with("slot=exhausted queue")));
    }

    #[test]
    fn audit_verifies_a_fresh_fully_evidenced_session() {
        let snapshot = json!({
            "fingerprint": "fp1",
            "policy": {"enforce_inventory": true, "operator_access": "local"},
            "sessions": [{
                "session_id": "s1", "name": "w1", "harness": "claude",
                "model": "glm", "model_basis": "verified",
                "account": "zai-main",
                "receipt_fingerprint": "fp1",
                "view_records": [{
                    "subject": "routing-view:s1", "lifecycle": "live",
                    "decision": "{\"view\": \"claude-native\", \"fingerprint\": \"fp1\", \"session_id\": \"s1\"}",
                }],
            }],
        });
        let (report, code) = audit_verify(&snapshot);
        assert_eq!(code, 0);
        assert_eq!(report["verdict"], "ROUTING_POLICY_VERIFIED");
        assert_eq!(report["sessions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn audit_names_the_boundary_when_only_a_preview_exists() {
        let snapshot = json!({
            "fingerprint": "fp1",
            "policy": {"enforce_inventory": true, "operator_access": "local"},
            "sessions": [],
        });
        let (report, code) = audit_verify(&snapshot);
        assert_eq!(code, 1);
        assert_eq!(report["verdict"], "ROUTING_POLICY_INCOMPLETE");
        let names: Vec<&str> = report["boundaries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["boundary"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"no-launch"));
    }

    #[test]
    fn audit_refuses_when_the_policy_is_not_armed() {
        let snapshot = json!({
            "fingerprint": "fp1",
            "policy": {"enforce_inventory": false, "operator_access": "local"},
            "sessions": [],
        });
        let (report, code) = audit_verify(&snapshot);
        assert_eq!(code, 1);
        let names: Vec<&str> = report["boundaries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["boundary"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"routing-not-enforced"));
    }

    #[test]
    fn audit_names_each_missing_evidence_boundary() {
        let session = |extra: Value| {
            let mut s = json!({
                "session_id": "s1", "name": "w1", "harness": "claude",
                "model": "glm", "model_basis": "verified",
                "account": "zai-main",
                "receipt_fingerprint": "fp1",
                "view_records": [{
                    "subject": "routing-view:s1", "lifecycle": "live",
                    "decision": "{\"view\": \"claude-native\", \"fingerprint\": \"fp1\", \"session_id\": \"s1\"}",
                }],
            });
            if let (Some(obj), Some(e)) = (s.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    obj.insert(k.clone(), v.clone());
                }
            }
            s
        };
        let snap = |sessions: Value| {
            json!({
                "fingerprint": "fp1",
                "policy": {"enforce_inventory": true, "operator_access": "local"},
                "sessions": sessions,
            })
        };
        let boundaries = |report: &Value| -> Vec<String> {
            report["boundaries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["boundary"].as_str().unwrap().to_string())
                .collect()
        };

        let mut s = session(json!({}));
        s["account"] = json!("");
        let (report, code) = audit_verify(&snap(json!([s])));
        assert_eq!(code, 1);
        assert!(boundaries(&report).contains(&"missing-account-identity".to_string()));

        let mut s = session(json!({}));
        s["receipt_fingerprint"] = json!("old");
        let (report, code) = audit_verify(&snap(json!([s])));
        assert_eq!(code, 1);
        assert!(boundaries(&report).contains(&"stale-receipt".to_string()));

        let mut s = session(json!({}));
        s["model_basis"] = json!("requested");
        let (report, code) = audit_verify(&snap(json!([s])));
        assert_eq!(code, 1);
        assert!(boundaries(&report).contains(&"unobserved-requested-model".to_string()));

        let mut s = session(json!({}));
        s["view_records"] = json!([]);
        let (report, code) = audit_verify(&snap(json!([s])));
        assert_eq!(code, 1);
        assert!(boundaries(&report).contains(&"missing-operator-view".to_string()));

        let mut s = session(json!({}));
        s["view_records"][0]["decision"] = json!(
            "{\"view\": \"claude-native\", \"fingerprint\": \"old\", \"session_id\": \"s1\"}"
        );
        let (report, code) = audit_verify(&snap(json!([s])));
        assert_eq!(code, 1);
        assert!(boundaries(&report).contains(&"view-fingerprint-mismatch".to_string()));
    }

    #[test]
    fn audit_reads_a_snapshot_file_and_prints_the_marker() {
        let snapshot = concat!(
            r#"{"fingerprint": "fp1","#,
            r#" "policy": {"enforce_inventory": true, "operator_access": "local"},"#,
            r#" "sessions": [{"session_id": "s1", "name": "w1", "harness": "claude","#,
            r#" "model": "glm", "model_basis": "verified", "account": "zai-main","#,
            r#" "receipt_fingerprint": "fp1", "view_records": [{"subject": "routing-view:s1","#,
            r#" "lifecycle": "live", "decision": "{\"view\": \"claude-native\", \"fingerprint\": \"fp1\", \"session_id\": \"s1\"}"}]}]}"#,
        );
        let dir = std::env::temp_dir().join(format!("fno-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("snapshot.json");
        std::fs::write(&path, snapshot).unwrap();
        let args: Vec<String> = vec![
            "audit".into(),
            "--project".into(),
            "/tmp/proj".into(),
            "--node".into(),
            "x-test".into(),
            "--json".into(),
            "--snapshot".into(),
            path.to_string_lossy().into_owned(),
        ];
        let (code, stdout, stderr) = run_route_slot_capture(&args);
        assert_eq!(stderr, "");
        assert_eq!(code, 0);
        assert!(stdout.contains("ROUTING_POLICY_VERIFIED"));
        let report: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["verdict"], "ROUTING_POLICY_VERIFIED");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn since_parser_reads_units_and_bare_seconds() {
        assert_eq!(parse_since_seconds("30m").unwrap(), 1800);
        assert_eq!(parse_since_seconds("2h").unwrap(), 7200);
        assert_eq!(parse_since_seconds("7d").unwrap(), 604800);
        assert_eq!(parse_since_seconds("90").unwrap(), 90);
        assert!(parse_since_seconds("abc").is_err());
    }

    #[test]
    fn loader_builds_sessions_from_the_machine_stores() {
        use crate::state::{Lineage, RegistryEntry};

        let dir = std::env::temp_dir().join(format!("fno-auditld-{}", std::process::id()));
        let agents_root = dir.join("agents");
        std::fs::create_dir_all(&agents_root).unwrap();
        let state_root = dir.clone();

        let fresh = chrono::Utc::now().to_rfc3339();
        let stale = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let entry = |sid: &str, created: String, node_v: &str| RegistryEntry {
            node: Some(node_v.to_string()),
            name: "worker-1".into(),
            harness: Some("claude".into()),
            cwd: "/tmp/proj".into(),
            project_root: "/tmp/proj".into(),
            created_at: created,
            model: Some("glm".into()),
            model_basis: Some("verified".into()),
            requested_model: Some("glm".into()),
            account_record_id: Some("zai-main".into()),
            harness_session_id: Some(sid.to_string()),
            fno_id: Some(sid.to_string()),
            ..RegistryEntry::new(Some(sid.to_string()), Lineage::captured((None, None, None)))
        };
        let registry = crate::state::Registry {
            entries: vec![
                entry("sid-1", fresh.clone(), "x-90a9"),
                entry("sid-old", stale, "x-90a9"),
            ],
            ..crate::state::Registry::default()
        };
        std::fs::write(
            &agents_root.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();

        std::fs::write(
            state_root.join("events.jsonl"),
            format!(
                "{{\"kind\":\"spawn_defaults_applied\",\"name\":\"worker-1\",\"fingerprint\":\"fp1\",\"ts\":\"{fresh}\"}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            state_root.join("decisions.jsonl"),
            format!(
                "{{\"ts\":\"{fresh}\",\"type\":\"operator_decision\",\"data\":{{\"decision_id\":\"d-view1\",\"subject\":\"routing-view:sid-1\",\"decision\":\"{{\\\"view\\\": \\\"claude-native\\\", \\\"fingerprint\\\": \\\"fp1\\\", \\\"session_id\\\": \\\"sid-1\\\"}}\"}}}}\n{{\"ts\":\"{fresh}\",\"type\":\"operator_decision\",\"data\":{{\"decision_id\":\"d-view2\",\"subject\":\"routing-view:sid-other\",\"decision\":\"x\"}}}}\n{{\"ts\":\"{fresh}\",\"type\":\"decision_retracted\",\"data\":{{\"target_decision_id\":\"d-view2\"}}}}\n{{\"ts\":\"{fresh}\",\"type\":\"operator_decision\",\"data\":{{\"decision_id\":\"d-view3\",\"subject\":\"routing-view:sid-1\",\"decision\":\"older record\",\"supersedes\":\"d-view1\"}}}}\n"
            ),
        )
        .unwrap();

        let facts = json!({
            "fingerprint": "fp1",
            "policy": {"enforce_inventory": true, "operator_access": "local"},
        });
        let snap = audit_load_snapshot(
            &facts,
            &state_root,
            &agents_root.join("registry.json"),
            "/tmp/proj",
            "x-90a9",
            1800,
        )
        .unwrap();
        assert_eq!(snap["fingerprint"], "fp1");
        let sessions = snap["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        assert_eq!(sessions[0]["session_id"], "sid-1");
        assert_eq!(sessions[0]["account"], "zai-main");
        assert_eq!(sessions[0]["receipt_fingerprint"], "fp1");
        assert_eq!(sessions[0]["view_records"].as_array().unwrap().len(), 1);
    }
}
