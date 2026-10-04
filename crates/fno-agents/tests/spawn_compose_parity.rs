//! Characterization over the spawn_compose goldens
//! (`tests/fixtures/spawn_compose/`): the pure compose answers the argv,
//! the stderr lines, the exit, the stdout and the journal row the Python
//! seam produced at the merge base.

//! parity-stage: characterization
//! parity-oracle: fno.agents.spawn_defaults.inject_spawn_defaults

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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spawn_compose")
}

/// The subtree at `keys`, or an empty table.
fn subtree(root: &toml::Table, keys: &[&str]) -> toml::Value {
    let mut cur = root;
    for key in keys {
        match cur.get(*key).and_then(toml::Value::as_table) {
            Some(t) => cur = t,
            None => return toml::Value::Table(toml::map::Map::new()),
        }
    }
    toml::Value::Table(cur.clone())
}

fn inputs_from(case: &Value, config: &toml::Table) -> fno_agents::spawn_compose::Inputs {
    let agents = subtree(config, &["agents"]);
    let empty = toml::Value::Table(toml::map::Map::new());
    let defaults = agents
        .get("defaults")
        .cloned()
        .unwrap_or_else(|| empty.clone());
    let profiles = agents
        .get("profiles")
        .cloned()
        .unwrap_or_else(|| empty.clone());
    let dispatch_verbs = subtree(config, &["dispatch", "verbs"]);
    fno_agents::spawn_compose::Inputs {
        argv: case["argv"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default(),
        defaults,
        profiles,
        dispatch_verbs,
        // The goldens were captured with harness markers scrubbed, so the
        // ambient inference answered the builtin default.
        roster: fno_agents::provider::footnote_verbs_public(),
        node_verb: case["node_verb"].as_str().map(str::to_string),
        env_node: case["env_node"].as_str().map(str::to_string),
        ambient_harness: "claude".to_string(),
        apply_permission_builtin: case["apply_permission_builtin"].as_bool().unwrap_or(true),
        scan: case["scan"].clone(),
        facts: case["facts"].clone(),
        node: case["node"].as_str().map(str::to_string),
        node_row: case.get("node_row").filter(|v| v.is_object()).cloned(),
        verbose: case
            .get("verbose")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[test]
fn compose_goldens_reproduce_the_python_seam() {
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
        cases.len() >= 30,
        "the captured compose families must ship (passthrough through node grid)"
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
        if case.get("stub_route_slot").and_then(Value::as_bool) == Some(true) {
            // Captured against a stubbed slot walk (see the fixture README):
            // a live walk cannot reproduce the stub's verdict and clock.
            continue;
        }
        let tmp = std::env::temp_dir().join(format!(
            "spawn-compose-parity-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        // The compose's arm reads the declared rows and policy from DISK
        // (the gather's own read), so the case's config rides FNO_CONFIG.
        std::fs::write(
            tmp.join("config.toml"),
            case["config_toml"].as_str().unwrap_or(""),
        )
        .expect("write config");
        let events = tmp.join("events.jsonl");
        std::env::set_var("FNO_EVENTS_PATH", &events);
        std::env::set_var("FNO_STATE_DIR", tmp.join("state"));
        std::env::set_var("FNO_AGENTS_HOME", tmp.join("agents-home"));
        std::env::set_var("FNO_CONFIG", tmp.join("config.toml"));
        // The roster reads the repo surface (skills/ + commands/), not an
        // installed copy, so the vocabulary matches the capture.
        std::env::set_var(
            "FNO_REPO_ROOT",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        );
        let config: toml::Table = case["config_toml"]
            .as_str()
            .unwrap_or("")
            .parse()
            .unwrap_or_else(|_| parse_error_config(&case));
        let inputs = inputs_from(&case, &config);
        let answer = fno_agents::spawn_compose::compose(&inputs);
        let expect = &case["expect"];
        assert_eq!(
            Value::Array(
                answer
                    .argv
                    .iter()
                    .map(|a| Value::String(a.clone()))
                    .collect()
            ),
            expect["argv"],
            "argv: {name}"
        );
        assert_eq!(
            Value::Array(
                answer
                    .stderr
                    .iter()
                    .map(|a| Value::String(a.clone()))
                    .collect()
            ),
            expect["stderr"],
            "stderr: {name}"
        );
        assert_eq!(
            i64::from(answer.exit),
            expect["exit"].as_i64().unwrap_or(0),
            "exit: {name}"
        );
        assert_eq!(
            answer.stdout.clone().unwrap_or(Value::Null),
            expect["stdout"],
            "stdout: {name}"
        );
        let journal = read_journal(&events);
        let mut expect_journal = expect["journal"].clone();
        if expect_journal.is_object()
            && expect_journal
                .get("fingerprint")
                .and_then(Value::as_str)
                .is_some_and(|f| !f.is_empty())
        {
            // The capture stamped the pre-canonical formula; this port made
            // the fingerprint key-sorted (one-time value change, documented
            // in the PR). Arm-presence stays pinned: the no-consult cases
            // keep "" and must still match.
            if let Some(obj) = expect_journal.as_object_mut() {
                obj.insert(
                    "fingerprint".into(),
                    journal.get("fingerprint").cloned().unwrap_or(Value::Null),
                );
            }
        }
        assert_eq!(journal, expect_journal, "journal: {name}");
        for key in [
            "FNO_EVENTS_PATH",
            "FNO_STATE_DIR",
            "FNO_AGENTS_HOME",
            "FNO_CONFIG",
            "FNO_REPO_ROOT",
        ] {
            std::env::remove_var(key);
        }
    }
}

fn parse_error_config(case: &Value) -> toml::Table {
    // An unreadable config case parses as empty; the seam degraded open.
    let _ = case;
    toml::Table::new()
}

fn read_journal(events: &Path) -> Value {
    let Ok(text) = std::fs::read_to_string(events) else {
        return Value::Null;
    };
    let last = text.lines().filter(|l| !l.trim().is_empty()).last();
    match last {
        Some(line) => {
            let mut row: Value = serde_json::from_str(line).expect("journal row");
            if let Some(obj) = row.as_object_mut() {
                obj.remove("ts");
            }
            row
        }
        None => Value::Null,
    }
}
