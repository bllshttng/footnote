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
    let walk_verdict = |payload: &Value| -> (Value, &'static str) {
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
        if !lane_rung.is_empty() {
            let lane = candidate
                .and_then(|c| c.get("lane"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (json!(format!("{lane_rung} {lane}")), "armed")
        } else {
            let terminal = slot_out
                .get("chain")
                .and_then(Value::as_array)
                .and_then(|c| c.last())
                .cloned()
                .unwrap_or(json!(""));
            (terminal, "unarmed")
        }
    };
    let (would_take, routing) = walk_verdict(payload);
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
                    return json!({
                        "status": "states", "lane_states": [], "chain": chain,
                        "on_exhausted": on_exhausted, "on_low": on_low,
                        "on_unknown": on_unknown,
                        "would_take": would_take, "routing": routing,
                    });
                }
            }
            if let Some(ovl_lanes) = ovl.get("lanes") {
                match ovl_lanes.as_array() {
                    Some(arr) if !arr.is_empty() => lanes_raw = Value::Array(arr.clone()),
                    _ => {
                        chain.push(json!(format!(
                            "slot=config {rung_base}.by_difficulty.{diff_key}.lanes must be a non-empty list when declared"
                        )));
                        return json!({
                            "status": "states", "lane_states": [], "chain": chain,
                            "on_exhausted": on_exhausted, "on_low": on_low,
                            "on_unknown": on_unknown,
                            "would_take": would_take, "routing": routing,
                        });
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
        return json!({
            "status": "states", "lane_states": [], "chain": chain,
            "would_take": would_take, "routing": "unarmed",
        });
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
            return json!({
                "status": "states", "lane_states": [], "chain": chain,
                "on_exhausted": on_exhausted, "on_low": on_low,
                "on_unknown": on_unknown,
                "would_take": would_take, "routing": routing,
            });
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
    json!({
        "status": "states",
        "lane_states": lane_states,
        "chain": chain,
        "on_exhausted": on_exhausted,
        "on_low": on_low,
        "on_unknown": on_unknown,
        "would_take": would_take,
        "routing": routing,
    })
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
        .unwrap_or("agents.profiles");
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

    let lanes_raw_value = payload.get("lanes_raw").cloned().unwrap_or(json!([]));
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
        // lanes here, so the grid's own occupied flag governs.
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
        return grid_leg(&payload, rung_base, &mut chain);
    }

    // An explicit model pin outranks the lanes (operator authority); it never
    // borrows a lane's harness or capacity. Config defaults do NOT outrank
    // lanes; only a typed flag does.
    if payload
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
    let (plan, rows, fields_by_rung) = match fold(rung_base, &lanes_arr, &declared_rows) {
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

    let mut demoted: Vec<(usize, String, String, String)> = Vec::new();
    let mut identity_skips: Vec<String> = Vec::new();
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
        );
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
        return none(chain);
    }
    chain.push(json!(format!("slot=exhausted {on_exhausted}")));
    none(chain)
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
    fn states_all_lanes_exhausted_reads_unarmed_with_the_terminal() {
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
        assert_eq!(out["routing"], "unarmed");
        assert!(out["would_take"].as_str().unwrap().contains("exhausted"));
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
}
