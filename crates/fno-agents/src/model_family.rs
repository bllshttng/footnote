//! A routing row may name a model family instead of a version. codex's own
//! model list, which codex refreshes, supplies the newest version at spawn.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Catalog {
    pub source: String,
    pub fetched_at: String,
    pub slugs: Vec<String>,
}

/// payload["codex_catalog"] = {"fetched_at", "slugs"} wins (the test seam,
/// honored the way an explicit payload capacity is); otherwise
/// load_catalog_at(codex_home()/models_cache.json).
pub fn load_catalog(payload: &Value) -> Result<Catalog, String> {
    match payload.get("codex_catalog") {
        Some(fixture) => catalog_from_fixture(fixture),
        None => {
            let home = crate::codex_store::codex_home()
                .ok_or_else(|| "no codex home: CODEX_HOME and HOME are both unset".to_string())?;
            load_catalog_at(&home.join("models_cache.json"))
        }
    }
}

fn catalog_from_fixture(value: &Value) -> Result<Catalog, String> {
    let obj = value.as_object().ok_or_else(|| {
        "<payload codex_catalog>: expected an object with fetched_at and slugs".to_string()
    })?;
    let slugs = obj
        .get("slugs")
        .and_then(Value::as_array)
        .ok_or_else(|| "<payload codex_catalog>: slugs must be a list".to_string())?
        .iter()
        .map(|s| {
            s.as_str()
                .map(str::to_string)
                .ok_or_else(|| "<payload codex_catalog>: slugs must be strings".to_string())
        })
        .collect::<Result<Vec<String>, String>>()?;
    let fetched_at = obj
        .get("fetched_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(Catalog {
        source: "payload codex_catalog".to_string(),
        fetched_at,
        slugs,
    })
}

/// Parse one models_cache.json: keep every models[].slug whose visibility is
/// not "hide". Err names the path and the cause (missing, unreadable, no
/// models list).
pub fn load_catalog_at(path: &std::path::Path) -> Result<Catalog, String> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{display}: {e}"))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{display}: {e}"))?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{display}: no models list"))?;
    let slugs = models
        .iter()
        .filter_map(|m| {
            let slug = m.get("slug")?.as_str()?;
            match m.get("visibility").and_then(Value::as_str) {
                Some("hide") => None,
                _ => Some(slug.to_string()),
            }
        })
        .collect();
    let fetched_at = value
        .get("fetched_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(Catalog {
        source: display,
        fetched_at,
        slugs,
    })
}

/// "gpt-6-luna" -> ([6], "luna"); "gpt-5.6-luna" -> ([5, 6], "luna"); None otherwise.
fn version_and_family(slug: &str) -> Option<(Vec<u32>, &str)> {
    let rest = slug.strip_prefix("gpt-")?;
    let (version, family) = rest.rsplit_once('-')?;
    if version.is_empty() || family.is_empty() {
        return None;
    }
    let mut list = Vec::new();
    for part in version.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        list.push(part.parse::<u32>().ok()?);
    }
    Some((list, family))
}

/// Numeric list comparison: missing ranks below present, so `6` beats `5.6`
/// and `6.1` beats `6`.
fn newer(a: &[u32], b: &[u32]) -> bool {
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// The listed slug gpt-<version>-<family> with the highest version.
pub fn newest_in_family<'a>(family: &str, catalog: &'a Catalog) -> Option<&'a str> {
    let mut best: Option<(&str, Vec<u32>)> = None;
    for slug in &catalog.slugs {
        if let Some((version, fam)) = version_and_family(slug) {
            if fam == family && best.as_ref().is_none_or(|(_, v)| newer(&version, v)) {
                best = Some((slug.as_str(), version));
            }
        }
    }
    best.map(|(slug, _)| slug)
}

/// A family word (a non-empty model value that is neither a listed slug nor
/// a versioned form) to its newest listed slug.
fn family_target(word: &str, catalog: &Catalog) -> Option<String> {
    let word = word.trim();
    if word.is_empty() || catalog.slugs.iter().any(|s| s == word) {
        return None;
    }
    newest_in_family(word, catalog).map(str::to_string)
}

fn resolve_row(row: &mut Value, name: &str, catalog: &Catalog, lines: &mut Vec<Value>) {
    if row.get("harness").and_then(Value::as_str).map(str::trim) != Some("codex") {
        return;
    }
    let model = row
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let Some(slug) = family_target(&model, catalog) else {
        return;
    };
    let obj = row.as_object_mut().expect("routing rows are objects");
    obj.insert("model".into(), json!(slug));
    obj.insert("family".into(), json!(model));
    lines.push(json!(format!(
        "slot family {name} {model} -> {slug} (codex models_cache.json fetched {})",
        catalog.fetched_at
    )));
}

/// A clone of the payload with every codex row's family word replaced, plus
/// the chain lines that say so. Rows: declared_rows values, inventory.rows
/// entries, and lanes_raw tables whose provider is codex. A value that is a
/// listed slug is a pin and stays. A value with no family match stays
/// verbatim. A resolved row also gets "family": "<word>". Reads the catalog
/// only when a codex row exists, and stores it under "codex_catalog" in the
/// clone so a later leg does not read the file again.
pub fn resolve_payload(payload: &Value) -> (Value, Vec<Value>) {
    let mut lines: Vec<Value> = Vec::new();
    let mut has_codex_row = false;
    if let Some(rows) = payload.get("declared_rows").and_then(Value::as_object) {
        has_codex_row |= rows
            .values()
            .any(|r| r.get("harness").and_then(Value::as_str) == Some("codex"));
    }
    if let Some(rows) = payload
        .get("inventory")
        .and_then(|i| i.get("rows"))
        .and_then(Value::as_array)
    {
        has_codex_row |= rows
            .iter()
            .any(|r| r.get("harness").and_then(Value::as_str) == Some("codex"));
    }
    if let Some(lanes) = payload.get("lanes_raw").and_then(Value::as_array) {
        has_codex_row |= lanes
            .iter()
            .any(|l| l.get("provider").and_then(Value::as_str) == Some("codex"));
    }
    if !has_codex_row {
        return (payload.clone(), lines);
    }
    let catalog = match load_catalog(payload) {
        Ok(c) => c,
        Err(err) => {
            lines.push(json!(format!(
                "slot family catalog unreadable ({err}); codex model values pass through verbatim"
            )));
            return (payload.clone(), lines);
        }
    };
    let mut out = payload.clone();
    if let Some(rows) = out.get_mut("declared_rows").and_then(Value::as_object_mut) {
        for (name, row) in rows.iter_mut() {
            resolve_row(row, name, &catalog, &mut lines);
        }
    }
    if let Some(rows) = out
        .get_mut("inventory")
        .and_then(|i| i.get_mut("rows"))
        .and_then(Value::as_array_mut)
    {
        for row in rows.iter_mut() {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            resolve_row(row, &name, &catalog, &mut lines);
        }
    }
    // Inline lane tables carry a closed vocabulary (slot_lanes.toml), so the
    // model value is replaced but no family key is added: fold would fault on
    // an unknown field.
    if let Some(lanes) = out.get_mut("lanes_raw").and_then(Value::as_array_mut) {
        let rung_base = payload
            .get("rung_base")
            .and_then(Value::as_str)
            .unwrap_or("agents.profiles");
        for (index, lane) in lanes.iter_mut().enumerate() {
            if lane.get("provider").and_then(Value::as_str) != Some("codex") {
                continue;
            }
            let model = lane
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let Some(slug) = family_target(&model, &catalog) else {
                continue;
            };
            if let Some(obj) = lane.as_object_mut() {
                obj.insert("model".into(), json!(slug));
            }
            lines.push(json!(format!(
                "slot family {rung_base}.lanes[{index}] {model} -> {slug} (codex models_cache.json fetched {})",
                catalog.fetched_at
            )));
        }
    }
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "codex_catalog".into(),
            json!({
                "source": catalog.source,
                "fetched_at": catalog.fetched_at,
                "slugs": catalog.slugs,
            }),
        );
    }
    (out, lines)
}

/// One line per codex row pinned to gpt-<v>-<fam> when a higher version of
/// <fam> is listed. Rows that carry "family" are skipped.
pub fn drift_lines(rows: &[Value], catalog: &Catalog) -> Vec<String> {
    let mut out = Vec::new();
    for row in rows {
        if row.get("family").is_some() {
            continue;
        }
        if row.get("harness").and_then(Value::as_str).map(str::trim) != Some("codex") {
            continue;
        }
        let model = row
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let Some((pinned, family)) = version_and_family(model) else {
            continue;
        };
        if !catalog.slugs.iter().any(|s| s == model) {
            continue;
        }
        let Some(newest) = newest_in_family(family, catalog) else {
            continue;
        };
        let Some((latest, _)) = version_and_family(newest) else {
            continue;
        };
        if newer(&latest, &pinned) {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("");
            out.push(format!(
                "drift {name} pins {model}; newest {family} in codex models_cache.json is {newest}"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- unit: version parsing and ordering ----

    #[test]
    fn version_and_family_splits_the_slug_and_orders_numerically() {
        assert_eq!(version_and_family("gpt-6-luna"), Some((vec![6], "luna")));
        assert_eq!(
            version_and_family("gpt-5.6-luna"),
            Some((vec![5, 6], "luna"))
        );
        assert_eq!(version_and_family("glm-5.3[1m]"), None);
        assert_eq!(version_and_family("gpt-reserve"), None);
        assert_eq!(version_and_family("gpt-5.5"), None);
        assert_eq!(version_and_family("codex-auto-review"), None);
    }

    #[test]
    fn newer_compares_version_lists_element_wise() {
        assert!(newer(&[6], &[5, 6]));
        assert!(newer(&[6, 1], &[6]));
        assert!(newer(&[5, 7], &[5, 6]));
        assert!(!newer(&[5, 6], &[6]));
        assert!(!newer(&[6], &[6]));
    }

    // ---- unit: catalog loading ----

    #[test]
    fn load_catalog_at_errors_naming_the_path_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models_cache.json");
        let err = load_catalog_at(&path).unwrap_err();
        assert!(err.contains(path.display().to_string().as_str()), "{err}");
    }

    #[test]
    fn load_catalog_at_filters_hidden_slugs_and_reads_fetched_at() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models_cache.json");
        std::fs::write(
            &path,
            json!({
                "fetched_at": "2026-09-23T06:14:54Z",
                "models": [
                    {"slug": "gpt-6-luna", "visibility": "list"},
                    {"slug": "gpt-reserve", "visibility": "hide"},
                    {"slug": "gpt-5.6-luna"},
                    {"slug": "codex-auto-review", "visibility": "hide"},
                ],
            })
            .to_string(),
        )
        .unwrap();
        let catalog = load_catalog_at(&path).unwrap();
        assert_eq!(catalog.fetched_at, "2026-09-23T06:14:54Z");
        assert_eq!(catalog.slugs, vec!["gpt-6-luna", "gpt-5.6-luna"]);
        assert!(catalog.source.contains("models_cache.json"));
    }

    #[test]
    fn load_catalog_prefers_the_payload_fixture() {
        let payload = json!({
            "codex_catalog": {
                "fetched_at": "2026-09-23T00:00:00Z",
                "slugs": ["gpt-5.6-luna", "gpt-6-luna"],
            },
        });
        let catalog = load_catalog(&payload).unwrap();
        assert_eq!(catalog.slugs.len(), 2);
        assert_eq!(catalog.fetched_at, "2026-09-23T00:00:00Z");
    }

    #[test]
    fn load_catalog_names_the_path_for_a_malformed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models_cache.json");
        std::fs::write(&path, "not json").unwrap();
        let err = load_catalog_at(&path).unwrap_err();
        assert!(err.contains(path.display().to_string().as_str()), "{err}");
    }

    // ---- unit: newest in family ----

    #[test]
    fn newest_in_family_compares_versions_numerically() {
        let catalog = Catalog {
            source: "test".into(),
            fetched_at: "t".into(),
            slugs: vec![
                "gpt-5.6-luna".into(),
                "gpt-6-luna".into(),
                "gpt-6.1-luna".into(),
                "gpt-6-sol".into(),
                "gpt-reserve".into(),
            ],
        };
        assert_eq!(newest_in_family("luna", &catalog), Some("gpt-6.1-luna"));
        assert_eq!(newest_in_family("sol", &catalog), Some("gpt-6-sol"));
        assert_eq!(newest_in_family("ghostfam", &catalog), None);
    }

    // ---- unit: resolve_payload ----

    fn catalog(slugs: &[&str]) -> Catalog {
        Catalog {
            source: "payload codex_catalog".into(),
            fetched_at: "2026-09-23T00:00:00Z".into(),
            slugs: slugs.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn resolve_payload_inner(payload: &Value, cat: &Catalog) -> (Value, Vec<Value>) {
        // Exercising resolve_payload with the fixture seam supplying the cat.
        let mut payload = payload.clone();
        payload.as_object_mut().unwrap().insert(
            "codex_catalog".into(),
            json!({"fetched_at": cat.fetched_at, "slugs": cat.slugs}),
        );
        resolve_payload(&payload)
    }

    #[test]
    fn resolve_payload_rewrites_codex_family_words_and_records_the_family() {
        let payload = json!({
            "rung_base": "agents.profiles.target",
            "lanes_raw": [
                "codex-luna",
                {"provider": "codex", "model": "terra"},
            ],
            "declared_rows": {
                "codex-luna": {"name": "codex-luna", "harness": "codex", "model": "luna"},
            },
            "inventory": {"rows": [
                {"name": "codex-sol", "harness": "codex", "model": "sol", "band": "high"},
            ]},
        });
        let cat = catalog(&["gpt-5.6-luna", "gpt-6-luna", "gpt-6-sol", "gpt-6-terra"]);
        let (resolved, lines) = resolve_payload_inner(&payload, &cat);
        let row = &resolved["declared_rows"]["codex-luna"];
        assert_eq!(row["model"], "gpt-6-luna");
        assert_eq!(row["family"], "luna");
        let inv = &resolved["inventory"]["rows"][0];
        assert_eq!(inv["model"], "gpt-6-sol");
        assert_eq!(inv["family"], "sol");
        let lane = &resolved["lanes_raw"][1];
        assert_eq!(lane["model"], "gpt-6-terra");
        // The inline lane vocabulary is closed; no family key may appear there.
        assert!(lane.get("family").is_none());
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|l| l
            .as_str()
            .unwrap()
            .starts_with("slot family codex-luna luna -> gpt-6-luna")));
        // The catalog rides the clone so a later leg does not read the file.
        assert_eq!(
            resolved["codex_catalog"]["slugs"].as_array().unwrap().len(),
            4
        );
    }

    #[test]
    fn resolve_payload_keeps_pins_and_unlisted_words_verbatim() {
        let payload = json!({
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["pin-x", "word-x", "claude-x"],
            "declared_rows": {
                "pin-x": {"name": "pin-x", "harness": "codex", "model": "gpt-5.6-luna"},
                "word-x": {"name": "word-x", "harness": "codex", "model": "ghostfam"},
                "claude-x": {"name": "claude-x", "harness": "claude", "model": "opus"},
            },
            "explicit_model_value": "luna",
        });
        let (resolved, lines) =
            resolve_payload_inner(&payload, &catalog(&["gpt-5.6-luna", "gpt-6-luna"]));
        assert_eq!(resolved["declared_rows"]["pin-x"]["model"], "gpt-5.6-luna");
        assert!(resolved["declared_rows"]["pin-x"].get("family").is_none());
        assert_eq!(resolved["declared_rows"]["word-x"]["model"], "ghostfam");
        assert_eq!(resolved["declared_rows"]["claude-x"]["model"], "opus");
        assert_eq!(resolved["explicit_model_value"], "luna");
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn resolve_payload_without_codex_rows_touches_nothing() {
        let payload = json!({
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["claude-x"],
            "declared_rows": {
                "claude-x": {"name": "claude-x", "harness": "claude", "model": "opus"},
            },
        });
        let (resolved, lines) = resolve_payload(&payload);
        assert_eq!(resolved, payload);
        assert!(lines.is_empty());
        assert!(resolved.get("codex_catalog").is_none());
    }

    #[test]
    fn resolve_payload_passes_through_with_one_unreadable_line_on_malformed_catalog() {
        let payload = json!({
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["codex-luna"],
            "declared_rows": {
                "codex-luna": {"name": "codex-luna", "harness": "codex", "model": "luna"},
            },
            "codex_catalog": "junk",
        });
        let (resolved, lines) = resolve_payload(&payload);
        assert_eq!(resolved["declared_rows"]["codex-luna"]["model"], "luna");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0]
            .as_str()
            .unwrap()
            .starts_with("slot family catalog unreadable"));
    }

    // ---- unit: drift lines ----

    #[test]
    fn drift_lines_flag_pinned_rows_below_newest_and_skip_resolved_rows() {
        let cat = catalog(&["gpt-5.6-luna", "gpt-6-luna", "gpt-5.6-terra"]);
        let rows = vec![
            json!({"name": "codex-luna", "harness": "codex", "model": "gpt-5.6-luna"}),
            // already resolved: carries family, never drifts
            json!({"name": "codex-luna", "harness": "codex", "model": "gpt-6-luna", "family": "luna"}),
            // pinned at the newest terra: no drift
            json!({"name": "codex-terra", "harness": "codex", "model": "gpt-5.6-terra"}),
            // not a codex row
            json!({"name": "claude-x", "harness": "claude", "model": "opus"}),
        ];
        let lines = drift_lines(&rows, &cat);
        assert_eq!(
            lines,
            vec!["drift codex-luna pins gpt-5.6-luna; newest luna in codex models_cache.json is gpt-6-luna".to_string()],
            "{lines:?}"
        );
    }

    // ---- integration: resolve_slot_payload (AC1-AC3) ----

    fn slot_payload(overrides: Value) -> Value {
        let mut base = json!({
            "rung_base": "agents.profiles.target",
            "lanes_raw": ["codex-luna"],
            "declared_rows": {
                "codex-luna": {"name": "codex-luna", "harness": "codex", "model": "luna"},
            },
            "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy",
                        "on_unknown": "allow", "by_difficulty": {}},
            "node": null,
            "capacity": {"codex": {"state": "ok", "window": "w",
                                   "accounts": {}, "evidence": {}, "resets": {}}},
            "vendor_counts": {}, "vendor_caps": {}, "vendor_count_errors": {},
            "thread_seatable": {}, "substrate": null, "permission_mode": null,
            "constrain_harness": null,
            "explicit_lane": false, "gate_bypassed": false,
            "codex_catalog": {
                "fetched_at": "2026-09-23T00:00:00Z",
                "slugs": ["gpt-5.6-luna", "gpt-6-luna"],
            },
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
    fn slot_resolves_family_words_in_declared_rows_ac1() {
        let out = crate::route_slot::resolve_slot_payload(&slot_payload(json!({})));
        assert_eq!(out["status"], "pick", "{}", out);
        assert_eq!(out["candidate"]["model"], "gpt-6-luna");
        assert_eq!(out["candidate"]["lane_fields"]["model"], "gpt-6-luna");
        let chain = chain_of(&out);
        assert!(
            chain
                .iter()
                .any(|l| l.starts_with("slot family codex-luna luna -> gpt-6-luna")),
            "{chain:?}"
        );
        // The terminal chain entry is preserved, not displaced.
        assert_eq!(
            chain.last().unwrap(),
            "slot agents.profiles.target.lanes[0] codex-luna capacity=ok window=w"
        );
    }

    #[test]
    fn slot_follows_a_newer_catalog_release_ac1() {
        let out = crate::route_slot::resolve_slot_payload(&slot_payload(json!({
            "codex_catalog": {
                "fetched_at": "2026-09-24T00:00:00Z",
                "slugs": ["gpt-5.6-luna", "gpt-6-luna", "gpt-7-luna"],
            },
        })));
        assert_eq!(out["candidate"]["model"], "gpt-7-luna");
    }

    #[test]
    fn slot_tier_leg_resolves_inventory_rows_ac1() {
        let out = crate::route_slot::resolve_slot_payload(&json!({
            "mode": "tier",
            "tier": "high",
            "provider": "codex",
            "inventory": {"rows": [
                {"name": "codex-sol", "harness": "codex", "model": "sol", "band": "high"},
            ]},
            "codex_catalog": {
                "fetched_at": "2026-09-23T00:00:00Z",
                "slugs": ["gpt-6-sol"],
            },
        }));
        assert_eq!(out["status"], "pick", "{}", out);
        assert_eq!(out["model"], "gpt-6-sol");
    }

    #[test]
    fn slot_fingerprint_ignores_catalog_differences_ac2() {
        // The fingerprint hashes the payload as declared; a model release
        // changes the catalog on disk, never the fingerprint. Two CODEX_HOME
        // catalogs that resolve differently must still print one fingerprint.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("CODEX_HOME").ok();
        let write_catalog = |dir: &std::path::Path, slugs: &[&str]| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("models_cache.json"),
                json!({
                    "fetched_at": "2026-09-23T00:00:00Z",
                    "models": slugs
                        .iter()
                        .map(|s| json!({"slug": s, "visibility": "list"}))
                        .collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .unwrap();
        };
        let tmp = tempfile::tempdir().unwrap();
        let home_a = tmp.path().join("a");
        let home_b = tmp.path().join("b");
        write_catalog(&home_a, &["gpt-5.6-luna", "gpt-6-luna"]);
        write_catalog(&home_b, &["gpt-5.6-luna", "gpt-6-luna", "gpt-7-luna"]);
        // No codex_catalog fixture: the disk seam is the only catalog source.
        let mut payload = slot_payload(json!({}));
        payload
            .as_object_mut()
            .unwrap()
            .remove("codex_catalog")
            .unwrap();
        let run = |home: &std::path::Path| {
            std::env::set_var("CODEX_HOME", home);
            crate::route_slot::resolve_slot_payload(&payload)
        };
        let out_a = run(&home_a);
        let out_b = run(&home_b);
        match saved {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        assert_eq!(out_a["candidate"]["model"], "gpt-6-luna");
        assert_eq!(out_b["candidate"]["model"], "gpt-7-luna");
        assert_eq!(out_a["fingerprint"], out_b["fingerprint"]);
    }

    #[test]
    fn slot_pin_claude_and_explicit_survive_ac2() {
        let payload = slot_payload(json!({
            "lanes_raw": ["pin-x", "word-x", "claude-x"],
            "declared_rows": {
                "pin-x": {"name": "pin-x", "harness": "codex", "model": "gpt-5.6-luna"},
                "word-x": {"name": "word-x", "harness": "codex", "model": "ghostfam"},
                "claude-x": {"name": "claude-x", "harness": "claude", "model": "opus"},
            },
            "capacity": {"codex": {"state": "ok", "window": "w",
                                   "accounts": {}, "evidence": {}, "resets": {}},
                         "claude": {"state": "ok", "window": "w",
                                    "accounts": {}, "evidence": {}, "resets": {}}},
        }));
        let resolved = resolve_payload(&payload).0;
        assert_eq!(resolved["declared_rows"]["pin-x"]["model"], "gpt-5.6-luna");
        assert_eq!(resolved["declared_rows"]["word-x"]["model"], "ghostfam");
        assert_eq!(resolved["declared_rows"]["claude-x"]["model"], "opus");
    }

    #[test]
    fn slot_malformed_catalog_passes_through_with_one_line_ac3() {
        let payload = slot_payload(json!({
            "codex_catalog": "junk",
        }));
        let out = crate::route_slot::resolve_slot_payload(&payload);
        assert_eq!(out["status"], "pick", "{}", out);
        assert_eq!(out["candidate"]["model"], "luna");
        let unreadable = chain_of(&out)
            .iter()
            .filter(|l| l.contains("slot family catalog unreadable"))
            .count();
        assert_eq!(unreadable, 1);
    }
}
