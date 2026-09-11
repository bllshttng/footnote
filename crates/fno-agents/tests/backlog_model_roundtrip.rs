//! Round-trip contract for the typed node model: every row of every golden
//! file and both live files round-trips through Node::from_json / to_json
//! equal to its input after null-valued keys are removed (AC8-HP). A key
//! with no column lands in extras, keeps its order, and shows up in the
//! census (AC9-EDGE).

use base64::Engine as _;
use fno_agents::backlog::model::Node;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Recursively remove null-valued object keys, the parity canonicalizer's
/// rule.
fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if !v.is_null() {
                    out.insert(k.clone(), strip_nulls(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_nulls).collect()),
        _ => Value::clone(value),
    }
}

/// Every typed-model row's extras keys with their row counts.
fn extras_census(rows: &[Value]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for row in rows {
        if let Ok(node) = Node::from_json(row) {
            for key in node.extras.keys() {
                *out.entry(key.clone()).or_insert(0) += 1;
            }
        }
    }
    out
}

/// The rows of one golden fixture's `file` (base64 of the raw store body).
fn golden_rows(path: &str) -> Vec<Value> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let file_b64 = doc["file"].as_str().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(file_b64)
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["entries"].as_array().cloned().unwrap_or_default()
}

fn live_rows(path: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["entries"].as_array().cloned().unwrap_or_default()
}

/// Round-trip one row: from_json, to_json, compare after null-stripping.
/// Returns Err describing the first divergence.
fn roundtrip(row: &Value) -> Result<(), String> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<no id>")
        .to_string();
    let node = Node::from_json(row).map_err(|e| format!("{id}: from_json: {e}"))?;
    let out = node.to_json();
    let want = strip_nulls(row);
    if out != want {
        // Name one differing key, descending one level into arrays, so the
        // failure is diagnosable.
        let out_obj = out.as_object().cloned().unwrap_or_default();
        let want_obj = want.as_object().cloned().unwrap_or_default();
        for k in want_obj.keys() {
            if out_obj.get(k) != want_obj.get(k) {
                if !out_obj.contains_key(k) {
                    return Err(format!(
                        "{id}: key {k:?} lost (input {:?})",
                        short(&want_obj[k])
                    ));
                }
                if let (Some(want_items), Some(out_items)) =
                    (want_obj[k].as_array(), out_obj[k].as_array())
                {
                    for (i, (w, o)) in want_items.iter().zip(out_items).enumerate() {
                        if w != o {
                            if let (Some(wm), Some(om)) = (w.as_object(), o.as_object()) {
                                for wk in wm.keys() {
                                    if wm.get(wk) != om.get(wk) {
                                        return Err(format!(
                                            "{id}: {k}[{i}].{wk} differs (in {:?} vs out {:?})",
                                            short(&wm[wk]),
                                            short(om.get(wk).unwrap_or(&Value::Null))
                                        ));
                                    }
                                }
                            }
                            return Err(format!(
                                "{id}: {k}[{i}] differs (in {:?} vs out {:?})",
                                short(w),
                                short(o)
                            ));
                        }
                    }
                }
                return Err(format!(
                    "{id}: key {k:?} differs (in {:?} vs out {:?})",
                    short(&want_obj[k]),
                    short(&out_obj[k])
                ));
            }
        }
        for k in out_obj.keys() {
            if !want_obj.contains_key(k) {
                return Err(format!("{id}: unexpected output key {k:?}"));
            }
        }
        return Err(format!("{id}: diverged (no single-key explanation)"));
    }
    Ok(())
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 90 {
        format!("{}...", &s[..90])
    } else {
        s
    }
}

#[test]
fn golden_files_round_trip() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/graph_store");
    let mut checked = 0;
    let entries = std::fs::read_dir(dir).unwrap();
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let rows = golden_rows(path.to_str().unwrap());
        for row in &rows {
            if let Err(problem) = roundtrip(row) {
                panic!("{}: {}", path.display(), problem);
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no golden rows were read");
}

/// AC9-EDGE: a key with no column lands in extras, survives to_json in its
/// original order, and is counted in the census.
#[test]
fn unknown_key_lands_in_extras_in_order() {
    let row: Value = serde_json::from_str(
        r#"{
        "id": "ab-extra",
        "slug": "extra",
        "title": "Extra",
        "type": "feature",
        "status": "idea",
        "priority": "p2",
        "created_at": "2026-09-11T00:00:00+00:00",
        "zz_first_unknown": "a",
        "model_tier": "opus",
        "aa_last_unknown": "b"
    }"#,
    )
    .unwrap();
    let node = Node::from_json(&row).unwrap();
    let keys: Vec<&String> = node.extras.keys().collect();
    assert_eq!(
        keys,
        vec!["zz_first_unknown", "model_tier", "aa_last_unknown"],
        "extras keeps insertion order"
    );
    let out = node.to_json();
    let serialized = out.to_string();
    let zz = serialized.find("zz_first_unknown").unwrap();
    let mt = serialized.find("model_tier").unwrap();
    let aa = serialized.find("aa_last_unknown").unwrap();
    assert!(
        zz < mt && mt < aa,
        "extras order survives export: {serialized}"
    );
    let counts = extras_census(std::slice::from_ref(&row));
    assert_eq!(counts.get("model_tier"), Some(&1));
}

/// The live soak: FNO_ROUNDTRIP_GRAPH names the file. Prints the AC line
/// `roundtrip: N rows, 0 diverged` plus the extras key census.
#[test]
#[ignore]
fn live_graph_round_trip() {
    let path = std::env::var("FNO_ROUNDTRIP_GRAPH").expect("set FNO_ROUNDTRIP_GRAPH");
    let rows = live_rows(&path);
    let mut diverged = 0;
    let mut failures: Vec<String> = Vec::new();
    for row in &rows {
        if let Err(problem) = roundtrip(row) {
            diverged += 1;
            if failures.len() < 10 {
                failures.push(problem);
            }
        }
    }
    println!("roundtrip: {} rows, {} diverged", rows.len(), diverged);
    for problem in &failures {
        println!("  {}", problem);
    }
    for (key, count) in extras_census(&rows) {
        println!("extras: {:6} {}", count, key);
    }
    assert_eq!(diverged, 0, "{} rows diverged", diverged);
}
