//! The config gather behind the route-slot verb, ported from Python
//! `fno.route_resolve` (`_slot_payload` and the inventory fold it called).
//! [`fill`] adds each config-derived payload key the caller left out; an
//! explicit key wins untouched, the rule `vendor_maps` follows, so the
//! walk's own 65 in-file tests over explicit payloads read the same answer.
//! The characterization goldens live in `tests/fixtures/route_gather/`.

use serde_json::{json, Map, Value};
use std::path::Path;

const MODEL_TIERS_TABLE: &str = include_str!("model_tiers.toml");

const SLOT_VERBS: [&str; 5] = ["think", "blueprint", "target", "review", "role"];
const OBJECTIVES: [&str; 3] = ["cheapest-that-clears", "best-available", "prefer-harness"];
/// A band's minimum coding percentile; a tier is a MINIMUM, so a model
/// clears it at the floor or above.
const BAND_FLOOR: [(&str, f64); 4] = [
    ("low", 50.0),
    ("medium", 70.0),
    ("high", 90.0),
    ("max", 95.0),
];

const DECLARED_FIELDS: [&str; 7] = [
    "harness",
    "model",
    "route",
    "account",
    "band",
    "effort",
    "operator_view",
];

fn tier_rank(band: &str) -> i32 {
    match band {
        "low" => 0,
        "medium" => 1,
        "high" => 2,
        "max" => 3,
        _ => -1,
    }
}

fn toml_str(v: Option<&toml::Value>) -> String {
    v.and_then(toml::Value::as_str).unwrap_or("").to_string()
}

fn toml_str_trim(v: Option<&toml::Value>) -> String {
    toml_str(v).trim().to_string()
}

/// The verb a payload's `rung_base` names: `agents.profiles.target` ->
/// `target`; the bare `agents.profiles` names no verb.
fn rung_verb(rung_base: &str) -> Option<String> {
    rung_base
        .strip_prefix("agents.profiles.")
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The config-declared rows exactly (never the built-in fallback): a
/// repeated name folds per field, never wholesale, and a config that does
/// not parse gathers as empty.
fn declared_rows(cwd: &Path) -> Value {
    let mut folded: Map<String, Value> = Map::new();
    if let Some(models) = crate::agents_config::config_value_deep(cwd, &["routing", "models"])
        .as_ref()
        .and_then(toml::Value::as_array)
    {
        for row in models {
            let name = toml_str_trim(row.get("name"));
            if name.is_empty() {
                continue;
            }
            // Every declared field is present, "" when no sighting named it:
            // "not named" reads as an empty string at the seam. A later row
            // overrides per field; its unset fields never clobber.
            let entry = folded.entry(name.clone()).or_insert_with(|| {
                let mut base = Map::new();
                base.insert("name".to_string(), json!(name));
                for field in DECLARED_FIELDS {
                    base.insert(field.to_string(), json!(""));
                }
                Value::Object(base)
            });
            for field in DECLARED_FIELDS {
                let value = toml_str_trim(row.get(field));
                if !value.is_empty() {
                    entry[field] = json!(value);
                }
            }
        }
    }
    Value::Object(folded)
}

/// The `{"on_exhausted", "on_low", "on_unknown", "by_difficulty"}` view of
/// one profile (or the empty-fields answer when the verb declares none).
fn profile_fields(table: Option<&toml::Table>) -> Value {
    let mut out = Map::new();
    for key in ["on_exhausted", "on_low", "on_unknown"] {
        out.insert(key.into(), json!(toml_str(table.and_then(|t| t.get(key)))));
    }
    let by_diff = table
        .and_then(|t| t.get("by_difficulty"))
        .and_then(toml::Value::as_table);
    let mut bd = Map::new();
    if let Some(bd_table) = by_diff {
        for (k, v) in bd_table {
            bd.insert(k.clone(), json!(v));
        }
    }
    out.insert("by_difficulty".into(), Value::Object(bd));
    Value::Object(out)
}

/// One lanes_raw entry: the lane rides VERBATIM (a string stays a string; a
/// table converts as-is). The padded vocabulary view is `slot_by_verb`'s
/// business, never the walk's input.
fn lanes_raw_entry(table: &toml::Value) -> Value {
    match table {
        toml::Value::String(name) => json!(name.as_str()),
        _ => serde_json::to_value(table).unwrap_or(Value::Null),
    }
}

/// `(profile_fields, lanes_raw)` for the payload's verb: the profile's own
/// lanes list, or `null` when the verb declares no profile at all (the shape
/// the slot lookup's None arm builds).
fn slot_entry(cwd: &Path, verb: Option<&str>) -> (Value, Value) {
    let table =
        verb.and_then(|v| crate::agents_config::config_value_deep(cwd, &["agents", "profiles", v]));
    match table.as_ref().and_then(toml::Value::as_table) {
        None => (profile_fields(None), Value::Null),
        Some(t) => {
            let profile = profile_fields(Some(t));
            let lanes = match t.get("lanes").and_then(toml::Value::as_array) {
                Some(items) => Value::Array(items.iter().map(lanes_raw_entry).collect()),
                None => Value::Null,
            };
            (profile, lanes)
        }
    }
}

/// Every verb slot_verbs reports, as JSON: the walk's strict-policy branch
/// picks the EFFECTIVE work kind's slot from this table. A verb with no
/// profile still gets a row (empty fields and `null` lanes), so a
/// configured profile the table omitted can never strict-refuse while its
/// lanes sat in config.
fn slot_profiles_table(cwd: &Path) -> Value {
    let mut out = Map::new();
    let mut verbs: Vec<String> = SLOT_VERBS.iter().map(|s| s.to_string()).collect();
    if let Some(profiles) = crate::agents_config::config_value_deep(cwd, &["agents", "profiles"])
        .as_ref()
        .and_then(toml::Value::as_table)
    {
        for key in profiles.keys() {
            if !verbs.iter().any(|v| v == key) {
                verbs.push(key.clone());
            }
        }
    }
    // A verb with no profile gets the empty row too (the doc comment's arm).
    for key in &verbs {
        let (profile, lanes) = slot_entry(cwd, Some(key));
        out.insert(
            key.clone(),
            json!({
                "rung_base": format!("agents.profiles.{key}"),
                "profile": profile,
                "lanes_raw": lanes,
                "has_overlay": profile["by_difficulty"]
                    .as_object()
                    .map(|m| !m.is_empty())
                    .unwrap_or(false),
            }),
        );
    }
    Value::Object(out)
}

/// `{"enforce_inventory": bool, "operator_access": lowercased-or-unknown}`.
fn policy(cwd: &Path) -> Value {
    let routing = crate::agents_config::config_value_deep(cwd, &["routing"]);
    let raw = toml_str(routing.as_ref().and_then(|r| r.get("operator_access")));
    let lowered = raw.trim().to_lowercase();
    let access = if lowered.is_empty() {
        "unknown".to_string()
    } else {
        lowered
    };
    json!({
        "enforce_inventory": routing
            .as_ref()
            .and_then(|r| r.get("enforce_inventory"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        "operator_access": access,
    })
}

/// `{account_id: vendor}` from the account records' `route` first segments.
/// The legacy `providers.records` alias answers the same question.
fn account_record_vendors(cwd: &Path) -> Value {
    let records = crate::agents_config::config_value_deep(cwd, &["accounts", "records"])
        .or_else(|| crate::agents_config::config_value_deep(cwd, &["providers", "records"]))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let mut out = Map::new();
    for record in &records {
        let id = toml_str(record.get("id"));
        let route = toml_str(record.get("route"));
        if id.is_empty() || route.trim().is_empty() {
            continue;
        }
        let vendor = route
            .replace(',', "/")
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        out.insert(id, json!(vendor));
    }
    Value::Object(out)
}

/// The snapshot at `<state_dir>/benchmarks.json`, or None when absent,
/// unreadable, or missing `fetched_at`/`source` (invalid is ignored, never
/// a dead gather).
fn load_snapshot(cwd: &Path) -> Option<Value> {
    let path = crate::agents_config::state_dir(cwd)?.join("benchmarks.json");
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let obj = data.as_object()?;
    if obj
        .get("fetched_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return None;
    }
    if obj
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return None;
    }
    Some(data)
}

/// The two tier tables from the canonical `model_tiers.toml`: strongest-band
/// names per band, and `name -> [harness, model]` reachability pairs.
fn load_model_tiers() -> (Vec<(String, Vec<String>)>, Map<String, Value>) {
    let raw: toml::Value = toml::from_str(MODEL_TIERS_TABLE).expect("model_tiers.toml must parse");
    let table_list = |key: &str| -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        if let Some(t) = raw.get(key).and_then(toml::Value::as_table) {
            for (band, names) in t {
                let list: Vec<String> = names
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                out.push((band.clone(), list));
            }
        }
        out
    };
    let mut reach = Map::new();
    if let Some(t) = raw.get("reachability").and_then(toml::Value::as_table) {
        for (name, pair) in t {
            let items: Vec<String> = pair
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            reach.insert(name.clone(), json!(items));
        }
    }
    (table_list("static_tiers"), reach)
}

fn band_from_percentile(pct: Option<f64>) -> String {
    // Python's ladder stops at high: a percentile never derives the max
    // band, and nothing sits above it to round up to.
    let Some(p) = pct else {
        return String::new();
    };
    let floor = |band: &str| {
        BAND_FLOOR
            .iter()
            .find(|(name, _)| *name == band)
            .map(|(_, floor)| *floor)
            .unwrap_or(0.0)
    };
    if p >= floor("high") {
        return "high".to_string();
    }
    if p >= floor("medium") {
        return "medium".to_string();
    }
    if p >= floor("low") {
        return "low".to_string();
    }
    String::new()
}

fn snapshot_percentiles(snapshot: Option<Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(models) = snapshot
        .as_ref()
        .and_then(|s| s.get("models"))
        .and_then(Value::as_array)
    {
        for entry in models {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let Some(pct) = entry.get("coding_percentile").and_then(Value::as_f64) else {
                continue;
            };
            out.insert(name.to_string(), json!(pct));
        }
    }
    out
}

/// The declared inventory folded over the built-in tier tables: builtin rows
/// first (name-sorted), then config rows in declared order, later rows of
/// one name overriding per field. A snapshot percentile derives a band only
/// when the row carries none of its own; the objective is validated against
/// the three-word vocabulary. Never fails: an unreadable piece answers empty.
fn inventory(cwd: &Path) -> Value {
    let (tiers, reach) = load_model_tiers();
    let cfg_rows: Vec<toml::Value> =
        crate::agents_config::config_value_deep(cwd, &["routing", "models"])
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
    let mut strongest: Map<String, Value> = Map::new();
    for (band, names) in &tiers {
        if tier_rank(band) < 0 {
            continue;
        }
        for name in names {
            let held = strongest.get(name).and_then(Value::as_str);
            let replace = held.map(|h| tier_rank(band) > tier_rank(h)).unwrap_or(true);
            if replace {
                strongest.insert(name.clone(), json!(band));
            }
        }
    }
    // Row order: builtins by name, then config rows in declared order; a
    // config row whose name a builtin already carried folds in place.
    // `order` starts EMPTY: add_row pushes every first sighting, so the
    // strongest-key seeding would double-count the builtins.
    let mut builtin_order: Vec<String> = strongest.keys().cloned().collect();
    builtin_order.sort();
    let mut order: Vec<String> = Vec::new();
    let mut folded: Map<String, Value> = Map::new();
    let mut add_row = |row: &toml::Value| {
        let name = toml_str_trim(row.get("name"));
        if name.is_empty() {
            return;
        }
        if !folded.contains_key(&name) {
            order.push(name.clone());
            folded.insert(name.clone(), json!({}));
        }
        let entry = folded.get_mut(&name).expect("inserted above");
        for key in [
            "name",
            "harness",
            "model",
            "route",
            "account",
            "band",
            "effort",
            "cost_per_mtok_in",
            "operator_view",
        ] {
            let value = match row.get(key) {
                Some(v) => v.clone(),
                None => continue,
            };
            // Python kept a value when it was neither None nor ""; TOML has
            // no null, so the only blank is an empty string.
            if value.as_str().map(str::trim) == Some("") {
                continue;
            }
            entry[key] = json!(value);
        }
    };
    for name in &builtin_order {
        let Some(pair) = reach.get(name).and_then(Value::as_array).cloned() else {
            continue;
        };
        if pair.len() != 2 {
            continue;
        }
        let band = strongest
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut row = toml::map::Map::new();
        row.insert("name".to_string(), toml::Value::String(name.clone()));
        row.insert(
            "harness".to_string(),
            toml::Value::String(pair[0].as_str().unwrap_or_default().to_string()),
        );
        row.insert(
            "model".to_string(),
            toml::Value::String(pair[1].as_str().unwrap_or_default().to_string()),
        );
        row.insert("band".to_string(), toml::Value::String(band));
        add_row(&toml::Value::Table(row));
    }
    for row in &cfg_rows {
        add_row(row);
    }
    build_inventory_answer(cwd, &order, &folded, !cfg_rows.is_empty())
}

/// The second half of the fold: one `asdict`-shaped row per name in fold
/// order, snapshot percentiles attached, band derived only when absent.
fn build_inventory_answer(
    cwd: &Path,
    order: &[String],
    folded: &Map<String, Value>,
    declared: bool,
) -> Value {
    let snap_pct = snapshot_percentiles(load_snapshot(cwd));
    let mut rows: Vec<Value> = Vec::new();
    for name in order {
        let merged = folded.get(name).cloned().unwrap_or(json!({}));
        let pct = snap_pct.get(name).and_then(Value::as_f64);
        let raw_band = merged
            .get("band")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let band = if BAND_FLOOR.iter().any(|(b, _)| *b == raw_band) {
            raw_band
        } else {
            band_from_percentile(pct)
        };
        let s = |key: &str| {
            merged
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        rows.push(json!({
            "name": name,
            "harness": s("harness"),
            "model": s("model"),
            "route": s("route"),
            "account": s("account"),
            "band": band,
            "percentile": pct.map(|p| json!(p)).unwrap_or(Value::Null),
            "effort": s("effort"),
            "cost_per_mtok_in": merged.get("cost_per_mtok_in").cloned().unwrap_or(Value::Null),
            "operator_view": s("operator_view"),
        }));
    }
    let objective = cfg_routing(cwd)
        .get("objective")
        .and_then(toml::Value::as_str)
        .unwrap_or("")
        .to_string();
    let objective = if OBJECTIVES.contains(&objective.as_str()) {
        objective
    } else {
        OBJECTIVES[0].to_string()
    };
    json!({
        "declared": declared,
        "objective": objective,
        "prefer_harness": toml_str_trim(cfg_routing(cwd).get("prefer_harness")),
        "rows": rows,
    })
}

fn cfg_routing(cwd: &Path) -> toml::Value {
    crate::agents_config::config_value_deep(cwd, &["routing"])
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()))
}

/// `{harness: thread-seatable}` over the payload's harness universe. An
/// undeclared harness degrades OPEN (true), the arm Python's try/except
/// answered; a declared harness reads its spawn claim.
fn thread_seatable_table(harnesses: &[String]) -> Value {
    let mut out = Map::new();
    for harness in dedup_first_seen(harnesses) {
        let seatable = !crate::effort_surface::is_declared(&harness)
            || crate::effort_surface::spawn_state(&harness) == "native";
        out.insert(harness.clone(), json!(seatable));
    }
    Value::Object(out)
}

/// The config gather: add each config-derived key the payload does not
/// already carry. An explicit key wins untouched (AC7-EDGE), so the walk's
/// own explicit-payload tests read the same answer filled or unfilled.
pub fn fill(payload: &Value, cwd: &Path) -> Value {
    let mut out = payload.clone();
    let Some(obj) = out.as_object_mut() else {
        return payload.clone();
    };
    if !obj.contains_key("declared_rows") {
        let rows = declared_rows(cwd);
        obj.insert("declared_rows".into(), rows);
    }
    let verb = obj
        .get("rung_base")
        .and_then(Value::as_str)
        .and_then(rung_verb);
    if !obj.contains_key("lanes_raw") || !obj.contains_key("profile") {
        let (profile, lanes) = slot_entry(cwd, verb.as_deref());
        if !obj.contains_key("profile") {
            obj.insert("profile".into(), profile);
        }
        if !obj.contains_key("lanes_raw") {
            obj.insert("lanes_raw".into(), lanes);
        }
    }
    if !obj.contains_key("policy") {
        let pol = policy(cwd);
        obj.insert("policy".into(), pol);
    }
    if !obj.contains_key("account_record_vendors") {
        let vendors = account_record_vendors(cwd);
        obj.insert("account_record_vendors".into(), vendors);
    }
    if !obj.contains_key("inventory") {
        let inv = inventory(cwd);
        obj.insert("inventory".into(), inv);
    }
    if !obj.contains_key("slot_by_verb") {
        let table = slot_profiles_table(cwd);
        obj.insert("slot_by_verb".into(), table);
    }
    // The three lookup tables read the FILLED payload's own universe:
    // gathered or explicit, the caller's answer is authoritative either way.
    let harnesses: Vec<String> = [
        declared_harness_list(obj),
        inventory_harness_list(obj),
        lanes_provider_list(obj),
    ]
    .concat();
    if !obj.contains_key("thread_seatable") {
        obj.insert("thread_seatable".into(), thread_seatable_table(&harnesses));
    }
    if !obj.contains_key("harness_installed") {
        obj.insert(
            "harness_installed".into(),
            harness_installed_table(&harnesses),
        );
    }
    if !obj.contains_key("effort_ok") {
        let rows: Vec<Value> = obj
            .get("inventory")
            .and_then(|i| i.get("rows"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        obj.insert("effort_ok".into(), effort_ok_table(&rows));
    }
    out
}

fn declared_harness_list(obj: &Map<String, Value>) -> Vec<String> {
    obj.get("declared_rows")
        .and_then(Value::as_object)
        .map(|rows| {
            rows.values()
                .filter_map(|row| {
                    row.get("harness")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn inventory_harness_list(obj: &Map<String, Value>) -> Vec<String> {
    obj.get("inventory")
        .and_then(|i| i.get("rows"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    row.get("harness")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn lanes_provider_list(obj: &Map<String, Value>) -> Vec<String> {
    obj.get("lanes_raw")
        .and_then(Value::as_array)
        .map(|lanes| {
            lanes
                .iter()
                .filter_map(|lane| {
                    lane.get("provider")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `{harness: installed}`: the harness roster is a shipped constant, so this
/// table never degrades.
fn harness_installed_table(harnesses: &[String]) -> Value {
    let mut out = Map::new();
    for harness in dedup_first_seen(harnesses) {
        let installed = crate::provider::KNOWN_PROVIDERS.contains(&harness.as_str());
        out.insert(harness.clone(), json!(installed));
    }
    Value::Object(out)
}

/// `{harness: {effort: ok}}` over the inventory rows, the effort surface's
/// own verdict per pair; rows with no harness or a blank effort are skipped.
fn effort_ok_table(inv_rows: &[Value]) -> Value {
    let mut out: Map<String, Value> = Map::new();
    for row in inv_rows {
        let harness = row.get("harness").and_then(Value::as_str).unwrap_or("");
        let effort = row.get("effort").and_then(Value::as_str).unwrap_or("");
        if harness.is_empty() || effort.trim().is_empty() {
            continue;
        }
        let verdict = crate::effort_surface::effort_tokens(harness, effort).is_ok();
        let entry = out.entry(harness.to_string()).or_insert_with(|| json!({}));
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(effort.to_string(), json!(verdict));
        }
    }
    Value::Object(out)
}

fn dedup_first_seen(items: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for item in items {
        if !seen.contains(item) {
            seen.push(item.clone());
        }
    }
    seen
}

/// The three gather modes, answered here and dispatched in
/// `run_route_slot_capture` before the walk. `inventory` returns the fold
/// plus the slot verbs; `policy` the strict-routing flags; `dispatch_model`
/// the precedence chain Python's `resolve_dispatch_model` ran.
pub fn run_mode(payload: &Value, cwd: &Path) -> Value {
    match payload.get("mode").and_then(Value::as_str).unwrap_or("") {
        "inventory" => {
            let inv = inventory(cwd);
            json!({
                "declared": inv["declared"],
                "objective": inv["objective"],
                "prefer_harness": inv["prefer_harness"],
                "rows": inv["rows"],
                "verbs": slot_verbs_list(cwd),
            })
        }
        "policy" => policy(cwd),
        "dispatch_model" => dispatch_model(payload, cwd),
        _ => json!({}),
    }
}

fn slot_verbs_list(cwd: &Path) -> Vec<Value> {
    let mut verbs: Vec<Value> = SLOT_VERBS.iter().map(|v| json!(v)).collect();
    if let Some(profiles) = crate::agents_config::config_value_deep(cwd, &["agents", "profiles"])
        .as_ref()
        .and_then(toml::Value::as_table)
    {
        for key in profiles.keys() {
            let key = key.as_str();
            if key.is_empty() {
                continue;
            }
            if !verbs.iter().any(|v| v.as_str() == Some(key)) {
                verbs.push(json!(key));
            }
        }
    }
    verbs
}

/// The precedence Python's `resolve_dispatch_model` ran: explicit, task pin,
/// task difficulty, plan pin, plan difficulty, provider default. Difficulty
/// rungs resolve through the tier leg, inventory attached.
fn dispatch_model(payload: &Value, cwd: &Path) -> Value {
    let s = |key: &str| -> Option<String> {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(v) = s("explicit") {
        return json!({"model": v, "source": "explicit", "chain": ["explicit"]});
    }
    if let Some(v) = s("task_model") {
        return json!({"model": v, "source": "task-pin", "chain": ["task-pin"]});
    }
    if let Some(v) = s("task_difficulty") {
        let (model, chain) = tier_leg(&v, payload, cwd);
        return json!({
            "model": model,
            "source": format!("task-difficulty({})", v.to_lowercase()),
            "chain": chain,
        });
    }
    if let Some(v) = s("plan_model") {
        return json!({"model": v, "source": "plan-default", "chain": ["plan-default"]});
    }
    if let Some(v) = s("plan_difficulty") {
        let (model, chain) = tier_leg(&v, payload, cwd);
        return json!({
            "model": model,
            "source": format!("plan-difficulty({})", v.to_lowercase()),
            "chain": chain,
        });
    }
    json!({
        "model": Value::Null,
        "source": "provider-default(no-difficulty)",
        "chain": ["provider-default(no-difficulty)".to_string()],
    })
}

/// The tier leg `dispatch_model` resolves through: one in-process call to
/// the walk's own `mode: "tier"` answer over the filled inventory.
fn tier_leg(tier: &str, payload: &Value, cwd: &Path) -> (Value, Vec<Value>) {
    let inv = inventory(cwd);
    let ask = json!({
        "mode": "tier",
        "tier": tier,
        "provider": payload.get("provider"),
        "inventory": inv,
    });
    let out = crate::route_slot::resolve_slot_payload(&ask);
    (
        out.get("model").cloned().unwrap_or(Value::Null),
        out.get("chain")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sandbox(name: &str) -> PathBuf {
        let tmp =
            std::env::temp_dir().join(format!("route-gather-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".fno")).unwrap();
        tmp
    }

    /// The gather modes run hermetic only under an env pin; these tests take
    /// the claims env lock (one env-mutating suite at a time).
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    type Saved = Vec<(&'static str, Option<std::ffi::OsString>)>;

    fn pinned_sandbox(
        name: &str,
        config: &str,
    ) -> (std::sync::MutexGuard<'static, ()>, PathBuf, Saved) {
        let guard = lock();
        let tmp = sandbox(name);
        std::fs::write(tmp.join(".fno/config.toml"), config).unwrap();
        // Config-path pins only, snapshot-then-restore: the state roots are
        // the world every sibling test resolves through, and a test that
        // moves them (set, removal or restore) sends that sibling down a
        // branch its assertions refuse.
        let saved: Saved = vec![
            (
                "FNO_GLOBAL_SETTINGS_PATH",
                std::env::var_os("FNO_GLOBAL_SETTINGS_PATH"),
            ),
            ("CODEX_HOME", std::env::var_os("CODEX_HOME")),
        ];
        std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", tmp.join("absent-global.json"));
        (guard, tmp, saved)
    }

    /// Restore the pins pinned_sandbox (and the CODEX_HOME seed) snapshot.
    fn clear_sandbox_env(saved: Saved) {
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn explicit_keys_are_never_gathered_again_ac7_edge() {
        let cwd = std::path::Path::new("/nonexistent-gather-cwd");
        let payload = json!({
            "rung_base": "agents.profiles.target",
            "declared_rows": {"x": {"name": "x"}},
            "inventory": {"rows": []},
        });
        let filled = fill(&payload, cwd);
        assert_eq!(filled["declared_rows"], payload["declared_rows"]);
        assert_eq!(filled["inventory"], payload["inventory"]);
        // The absent keys still gather (an empty world answers empty).
        assert!(filled["policy"].is_object());
    }

    #[test]
    fn dispatch_model_precedence_chain_ac8_hp() {
        let payload = json!({
            "mode": "dispatch_model",
            "task_difficulty": "HIGH",
            "plan_model": "fallback-m",
        });
        let (guard, tmp, saved) = pinned_sandbox("dispatch-precedence", "");
        // The tier leg consults the codex catalog when it resolves a codex
        // model; a seeded empty catalog keeps the chain free of the
        // catalog-unreadable notice this sandbox cannot fetch.
        let codex_home = tmp.join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(codex_home.join("models_cache.json"), r#"{"models": []}"#).unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);
        let out = run_mode(&payload, &tmp);
        assert_eq!(out["source"], "task-difficulty(high)");
        // The chain carries the tier leg's own head vocabulary.
        assert_eq!(out["chain"][0], "tier(high)");
        // A plan pin outranks a plan difficulty but not the task rungs.
        let out = run_mode(
            &json!({"mode": "dispatch_model", "plan_model": "pm", "plan_difficulty": "low"}),
            &tmp,
        );
        assert_eq!(out["source"], "plan-default");
        let out = run_mode(&json!({"mode": "dispatch_model"}), &tmp);
        assert_eq!(out["source"], "provider-default(no-difficulty)");
        drop(guard);
        clear_sandbox_env(saved);
    }

    #[test]
    fn policy_mode_answers_both_flags_ac8_hp() {
        let (guard, tmp, saved) = pinned_sandbox(
            "policy",
            "[routing]\nenforce_inventory = true\noperator_access = \"Local\"\n",
        );
        let out = run_mode(&json!({"mode": "policy"}), &tmp);
        drop(guard);
        clear_sandbox_env(saved);
        assert_eq!(out["enforce_inventory"], json!(true));
        assert_eq!(out["operator_access"], "local");
    }

    #[test]
    fn unparseable_config_answers_empty_ac8_err() {
        let (guard, tmp, saved) = pinned_sandbox("unparseable", "not [ valid {{{{");
        let inv = run_mode(&json!({"mode": "inventory"}), &tmp);
        let pol = run_mode(&json!({"mode": "policy"}), &tmp);
        drop(guard);
        clear_sandbox_env(saved);
        assert_eq!(inv["declared"], json!(false));
        assert_eq!(inv["objective"], "cheapest-that-clears");
        // An unparseable config degrades to the BUILT-IN rows (a tier
        // request stays answerable); declared stays false.
        assert!(
            inv["rows"].as_array().is_some_and(|rows| !rows.is_empty()),
            "the built-in rows answer an unreadable config"
        );
        assert_eq!(pol["enforce_inventory"], json!(false));
        assert_eq!(pol["operator_access"], "unknown");
    }
}

/// `spawn_compose`'s view of the three helpers the walk ask needs.
pub(crate) fn policy_for(cwd: &Path) -> Value {
    policy(cwd)
}

pub(crate) fn profile_fields_for(table: Option<&toml::Table>) -> Value {
    profile_fields(table)
}

pub(crate) fn lane_entry_for(table: &toml::Value) -> Value {
    lanes_raw_entry(table)
}

/// Keys sorted recursively: two legs building the same content must hash the
/// same, whatever order each inserted its keys in.
pub(crate) fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), canonical(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}
