//! The `fno doctor route` row table, ported from route_cli.py. The Rust
//! answer carries the rows, the refusals and the drift lines; Python prints
//! them. Verdict order matches the loop it replaces exactly.

use serde_json::{json, Value};

/// The `mode: "inventory"` leg: `{"rows": [...], "refusals": [...],
/// "drift": [...]}`. Rows arrive already family-resolved by the pre-pass.
pub fn inventory_leg(payload: &Value) -> Value {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let routing_rows = crate::agents_config::config_lookup(&cwd, &["routing", "models"])
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    inventory_leg_with(payload, &routing_rows)
}

/// Testable body of [`inventory_leg`]: the same answer over handed-in routing
/// rows, so unit tests stay hermetic (no config on disk).
pub fn inventory_leg_with(payload: &Value, routing_rows: &[toml::Value]) -> Value {
    let rows_in: Vec<Value> = payload
        .get("inventory")
        .and_then(|i| i.get("rows"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let known: Vec<String> = payload
        .get("known_harnesses")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut named: Vec<&Value> = rows_in.iter().collect();
    named.sort_by_key(|r| {
        r.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    let mut rows = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    for row in named {
        let get = |k: &str| {
            row.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let name = get("name");
        let harness = get("harness");
        let model = get("model");
        let band = get("band");
        let effort = get("effort");
        let percentile = row.get("percentile").and_then(Value::as_f64);
        // The verdict order is the loop route_cli.py used to run: a row that
        // answers nothing is incomplete before it is anything else.
        let verdict = if harness.is_empty() || model.is_empty() {
            "incomplete"
        } else if !known.iter().any(|k| *k == harness) {
            refusals.push(format!(
                "{name}: harness '{harness}' is not installed (known: {})",
                known.join(", ")
            ));
            "not-installed"
        } else if band.is_empty() {
            "unbanded"
        } else {
            "ok"
        };
        let reading = crate::context_window::window_from_rows(&model, routing_rows);
        let window = match reading.measured_at {
            Some(date) => format!("{} (measured {})", reading.tokens, date),
            None => format!("unmeasured, default {}", reading.tokens),
        };
        let model_cell = match row.get("family").and_then(Value::as_str) {
            Some(family) if !family.is_empty() => format!("{family} -> {model}"),
            _ => model,
        };
        rows.push(json!({
            "name": name,
            "harness": harness,
            "model": model_cell,
            "band": if band.is_empty() { json!("unbanded") } else { json!(band) },
            // f64 Display prints the shortest round-trip decimal, not `:g`.
            "percentile": percentile.map(|p| format!("{p}")).unwrap_or_default(),
            "effort": effort,
            "window": window,
            "verdict": verdict,
        }));
    }
    let drift = crate::model_family::load_catalog(payload)
        .ok()
        .map(|catalog| crate::model_family::drift_lines(&rows_in, &catalog))
        .unwrap_or_default();
    json!({"rows": rows, "refusals": refusals, "drift": drift})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KNOWN: &[&str] = &["claude", "codex", "opencode"];

    fn leg_with(rows: Value, extra: Value) -> Value {
        let mut payload = json!({
            "inventory": {"rows": rows},
            "known_harnesses": KNOWN,
        });
        for (k, v) in extra.as_object().unwrap() {
            payload
                .as_object_mut()
                .unwrap()
                .insert(k.clone(), v.clone());
        }
        inventory_leg(&payload)
    }

    #[test]
    fn verdicts_follow_the_ported_order_and_sort_by_name_ac6() {
        let out = leg_with(
            json!([
                {"name": "mystery", "harness": "claude", "model": "m", "band": "", "effort": ""},
                {"name": "b-x", "harness": "codex", "model": "gpt-6-luna", "band": "high", "effort": "high"},
                {"name": "ghost-x", "harness": "ghostharness", "model": "g", "band": "high", "effort": ""},
                {"name": "a-x", "harness": "claude", "model": "", "band": "high", "effort": ""},
            ]),
            json!({}),
        );
        let rows = out["rows"].as_array().unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["a-x", "b-x", "ghost-x", "mystery"]);
        let verdicts: Vec<&str> = rows
            .iter()
            .map(|r| r["verdict"].as_str().unwrap())
            .collect();
        assert_eq!(
            verdicts,
            vec!["incomplete", "ok", "not-installed", "unbanded"]
        );
        // An unbanded row renders the unbanded band word, never "".
        assert_eq!(rows[3]["band"], "unbanded");
        let refusals: Vec<String> = out["refusals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            refusals,
            vec![
                "ghost-x: harness 'ghostharness' is not installed (known: claude, codex, opencode)"
            ]
        );
    }

    #[test]
    fn percentile_renders_the_shortest_decimal() {
        let out = leg_with(
            json!([
                {"name": "a", "harness": "claude", "model": "m", "band": "high",
                 "percentile": 90.0, "effort": ""},
                {"name": "b", "harness": "claude", "model": "m", "band": "high",
                 "percentile": 87.5, "effort": ""},
                {"name": "c", "harness": "claude", "model": "m", "band": "high", "effort": ""},
            ]),
            json!({}),
        );
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows[0]["percentile"], "90");
        assert_eq!(rows[1]["percentile"], "87.5");
        assert_eq!(rows[2]["percentile"], "");
    }

    #[test]
    fn family_row_renders_the_cell_and_skips_drift_ac5() {
        let out = leg_with(
            json!([
                {"name": "codex-luna", "harness": "codex", "model": "gpt-6-luna",
                 "band": "high", "family": "luna", "effort": ""},
            ]),
            json!({"codex_catalog": {"fetched_at": "t", "slugs": ["gpt-5.6-luna", "gpt-6-luna"]}}),
        );
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows[0]["model"], "luna -> gpt-6-luna");
        assert!(out["drift"].as_array().unwrap().is_empty(), "{}", out);
    }

    #[test]
    fn pinned_below_newest_carries_the_drift_line_ac4() {
        let out = leg_with(
            json!([
                {"name": "codex-luna", "harness": "codex", "model": "gpt-5.6-luna",
                 "band": "high", "effort": ""},
            ]),
            json!({"codex_catalog": {"fetched_at": "t", "slugs": ["gpt-5.6-luna", "gpt-6-luna"]}}),
        );
        assert_eq!(
            out["drift"],
            json!(["drift codex-luna pins gpt-5.6-luna; newest luna in codex models_cache.json is gpt-6-luna"])
        );
    }

    #[test]
    fn pinned_at_newest_drifts_nothing_ac5() {
        let out = leg_with(
            json!([
                {"name": "codex-terra", "harness": "codex", "model": "gpt-5.6-terra",
                 "band": "medium", "effort": ""},
            ]),
            json!({"codex_catalog": {"fetched_at": "t", "slugs": ["gpt-5.6-terra"]}}),
        );
        assert!(out["drift"].as_array().unwrap().is_empty());
    }

    #[test]
    fn empty_inventory_answers_three_empty_lists() {
        let out = inventory_leg(&json!({"mode": "inventory", "known_harnesses": KNOWN}));
        assert_eq!(out["rows"], json!([]));
        assert_eq!(out["refusals"], json!([]));
        assert_eq!(out["drift"], json!([]));
    }

    #[test]
    fn window_cell_shows_the_dated_row_or_the_unmeasured_default_ac7() {
        let routing: Vec<toml::Value> = toml::from_str::<toml::Value>(
            "[[rows]]\nmodel = \"glm-5.3-flash[1m]\"\ncontext = 1310720\ncontext_measured_at = \"2026-09-17\"\n",
        )
        .unwrap()
        .get("rows")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap();
        let out = inventory_leg_with(
            &json!({
                "inventory": {"rows": [
                    {"name": "flash", "harness": "claude", "model": "glm-5.3-flash", "band": "medium", "effort": ""},
                    {"name": "other", "harness": "codex", "model": "gpt-6-luna", "band": "high", "effort": ""},
                ]},
                "known_harnesses": KNOWN,
            }),
            &routing,
        );
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows[0]["window"], "1310720 (measured 2026-09-17)");
        assert_eq!(
            rows[1]["window"],
            format!(
                "unmeasured, default {}",
                crate::context_window::window_for_model("gpt-6-luna")
            )
        );
    }
}
