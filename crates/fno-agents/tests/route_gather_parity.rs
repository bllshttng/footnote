//! Characterization over the route_gather goldens
//! (`tests/fixtures/route_gather/`): the filled payload must equal the
//! payload Python's gather sent the verb, as a JSON value, and the
//! fingerprint the walk stamps must be equal for both.

//! parity-stage: characterization
//! parity-oracle: fno.route_resolve._slot_payload

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/route_gather")
}

/// Pin the config world to one tmp dir per case: FNO_CONFIG as the sole
/// candidate, an absent global tier, empty agents home and state.
fn pin_env(tmp: &Path) {
    std::env::set_var("FNO_CONFIG", tmp.join("config.toml"));
    std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", tmp.join("absent-global.json"));
    std::env::set_var("FNO_AGENTS_HOME", tmp.join("agents-home"));
    std::env::set_var("FNO_STATE_DIR", tmp.join("state"));
    // The capture ran with the canonical tier off; keep the read hermetic.
    std::env::set_var("FNO_NO_CANONICAL_CONFIG", "1");
}

fn unpin_env() {
    for key in [
        "FNO_CONFIG",
        "FNO_GLOBAL_SETTINGS_PATH",
        "FNO_AGENTS_HOME",
        "FNO_STATE_DIR",
        "FNO_NO_CANONICAL_CONFIG",
    ] {
        std::env::remove_var(key);
    }
}

#[test]
fn gather_goldens_reproduce_the_python_payload() {
    let _guard = env_lock();
    let dir = fixture_dir();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixture dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
            if is_json {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    cases.sort();
    assert!(
        cases.len() >= 3,
        "the three captured configs must ship: no-inventory, declared, strict"
    );
    for case_path in &cases {
        let name = case_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let case: Value =
            serde_json::from_str(&std::fs::read_to_string(case_path).expect("read case"))
                .expect("parse case");
        let tmp = std::env::temp_dir().join(format!(
            "route-gather-parity-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        std::fs::write(
            tmp.join("config.toml"),
            case["config_toml"].as_str().unwrap_or(""),
        )
        .expect("write config");
        std::fs::create_dir_all(tmp.join("agents-home")).expect("agents home");
        std::fs::create_dir_all(tmp.join("state")).expect("state dir");
        pin_env(&tmp);
        let transport = case["transport_payload"].clone();
        let filled = fno_agents::route_gather::fill(&transport, &tmp);
        let expect = case["expect_payload"].clone();
        assert_eq!(filled, expect, "payload equality: {name}");
        // The walk stamps the same fingerprint over both payloads.
        let fp_filled = fno_agents::route_slot::resolve_slot_payload(&filled);
        let fp_expect = fno_agents::route_slot::resolve_slot_payload(&expect);
        let get_fp = |answer: &Value| {
            answer
                .get("fingerprint")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(
            get_fp(&fp_filled),
            get_fp(&fp_expect),
            "fingerprint: {name}"
        );
        let inv_expected = &case["expect_inventory"];
        assert_eq!(
            filled.get("inventory").unwrap_or(&Value::Null),
            inv_expected,
            "inventory fold: {name}"
        );
        unpin_env();
    }

    // The node-pin legs (route_node_pin): a node's own pin overrides the
    // lanes one rung below a typed flag, and the chain names which pin
    // decided. The fixtures here name every input the legs read, so the
    // asserts stay hermetic even though the env is unpinned by now.
    let pin_pick = fno_agents::route_slot::resolve_slot_payload(&serde_json::json!({
        "policy": {"enforce_inventory": true, "operator_access": "unknown"},
        "work_verb": "target",
        "node": {"difficulty": "medium", "model": "claude-opus-5", "effort": "high"},
        "slot_by_verb": {
            "target": {
                "rung_base": "agents.profiles.target",
                "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                "lanes_raw": ["opus-x"],
            },
        },
        "declared_rows": {
            "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                       "operator_view": "claude-native"},
        },
        "effort_ok": {"claude": {"high": true}},
    }));
    assert_eq!(pin_pick["status"], "pick");
    assert_eq!(pin_pick["candidate"]["model"], "claude-opus-5");
    assert_eq!(pin_pick["candidate"]["harness"], "claude");
    assert_eq!(pin_pick["candidate"]["effort"], "high");
    let chain: Vec<String> = pin_pick["chain"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(chain.iter().any(|l| l.contains("slot=node-pin row=opus-x")));

    let fallback = fno_agents::route_slot::resolve_slot_payload(&serde_json::json!({
        "policy": {"enforce_inventory": true, "operator_access": "unknown"},
        "work_verb": "target",
        "node": {"difficulty": "medium", "model": "mystery-model"},
        "slot_by_verb": {
            "target": {
                "rung_base": "agents.profiles.target",
                "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                "lanes_raw": ["flash-x"],
            },
        },
        "declared_rows": {
            "flash-x": {"name": "flash-x", "harness": "claude", "model": "glm",
                        "route": "zai/glm", "account": "zai-main",
                        "operator_view": "claude-native"},
        },
        "capacity": {"claude": {"state": "ok", "window": "w", "accounts": {}, "evidence": {}, "resets": {}}},
    }));
    let chain: Vec<String> = fallback["chain"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(chain
        .iter()
        .any(|l| l.contains("no routing.models row names it")));

    let typed_wins = fno_agents::route_slot::resolve_slot_payload(&serde_json::json!({
        "policy": {"enforce_inventory": true, "operator_access": "unknown"},
        "work_verb": "target",
        "node": {"difficulty": "medium", "model": "claude-opus-5"},
        "slot_by_verb": {
            "target": {
                "rung_base": "agents.profiles.target",
                "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                "lanes_raw": ["flash-x"],
            },
        },
        "declared_rows": {
            "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5"},
            "sol-x": {"name": "sol-x", "harness": "codex", "model": "gpt-strong"},
        },
        "explicit_model_value": "gpt-strong",
    }));
    assert_eq!(typed_wins["status"], "pick");
    assert_eq!(typed_wins["candidate"]["harness"], "codex");
    let chain: Vec<String> = typed_wins["chain"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(chain
        .iter()
        .any(|l| l.contains("slot=operator-pin-override")));

    let effort_rides = fno_agents::route_slot::resolve_slot_payload(&serde_json::json!({
        "policy": {"enforce_inventory": true, "operator_access": "unknown"},
        "work_verb": "target",
        "node": {"difficulty": "medium", "effort": "high"},
        "slot_by_verb": {
            "target": {
                "rung_base": "agents.profiles.target",
                "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                "lanes_raw": ["opus-x"],
            },
        },
        "declared_rows": {
            "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                       "operator_view": "claude-native"},
        },
        "capacity": {"claude": {"state": "ok", "window": "w", "accounts": {}, "evidence": {}, "resets": {}}},
        "effort_ok": {"claude": {"high": true}},
    }));
    assert_eq!(effort_rides["status"], "pick");
    assert_eq!(effort_rides["candidate"]["effort"], "high");

    let no_surface = fno_agents::route_slot::resolve_slot_payload(&serde_json::json!({
        "policy": {"enforce_inventory": true, "operator_access": "unknown"},
        "work_verb": "target",
        "node": {"difficulty": "medium", "effort": "high"},
        "slot_by_verb": {
            "target": {
                "rung_base": "agents.profiles.target",
                "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy", "on_unknown": "allow"},
                "lanes_raw": ["opus-x"],
            },
        },
        "declared_rows": {
            "opus-x": {"name": "opus-x", "harness": "claude", "model": "claude-opus-5",
                       "operator_view": "claude-native"},
        },
        "capacity": {"claude": {"state": "ok", "window": "w", "accounts": {}, "evidence": {}, "resets": {}}},
        "effort_ok": {},
    }));
    assert_eq!(no_surface["status"], "pick");
    assert!(no_surface["candidate"]["effort"].is_null());
}
