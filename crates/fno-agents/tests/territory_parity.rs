//! parity-stage: differential
//! parity-oracle: fno.active_backlog.drain_targets_as_dicts, fno.active_backlog.territory_rows, fno.worker.blueprint.blueprint_feed
//!
//! Differential parity for the territory port (x-e221): the Python legs and
//! the Rust fact set (`active_backlog::native_receipt` /
//! `territory::territory_rows` / `blueprint_feed_status`) run over identical
//! graph + config + registry fixtures and must agree.
//!
//! FNO_CAPTURE_GOLDEN=1 runs the Python leg, asserts Rust==Python, and
//! freezes the goldens under tests/golden/territory/ - step 2 of the port
//! protocol (docs/architecture/dual-implementation-inventory.md), run BEFORE
//! any Python leg is deleted. After the deletion this file converts to
//! parity-stage: characterization and the goldens are the contract.
//!
//! One field is normalized on both sides before compare: the feed receipt's
//! worker_name_next embeds a label with a per-scope digest (the Python leg
//! used sha1, the port sha256; only per-scope stability is load-bearing, so
//! the digest is masked, never compared).

use common::{assert_golden as assert_golden_common, capture_mode, Golden};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn python_executable() -> PathBuf {
    let venv = pythonpath().join("../.venv/bin/python");
    if venv.is_file() {
        venv
    } else {
        PathBuf::from("python3")
    }
}

/// Serialize FNO_CONFIG/FNO_HOME mutation across the parallel test threads.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn python_available() -> bool {
    let probe = Command::new(python_executable())
        .arg("-c")
        .arg("import fno.active_backlog, fno.worker.blueprint")
        .env("PYTHONPATH", pythonpath())
        .output();
    matches!(probe, Ok(o) if o.status.success())
}

// -------------------------------------------------------------------------
// The shared fixture: one tempdir holding config.toml (state_dir pointed
// back into it), graph.json, and the agents registry both legs read.
// -------------------------------------------------------------------------

struct Fixture {
    tmp: tempfile::TempDir,
    registry: PathBuf,
}

fn build_fixture(active_backlog_extra: &str, graph: Value, registry_rows: Value) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_string_lossy().replace('\'', "");
    // state_dir points into the fixture so both legs resolve the graph, the
    // registry, and the blueprinter records inside it. active_backlog_extra
    // folds into the same table (never a second header, TOML forbids it).
    let config = format!(
        "state_dir = '{root}'\n[active_backlog]\nenabled = true\ninterval = \"5m\"\nfailure_limit = 3\nmax_concurrent = 2\n{active_backlog_extra}[[work.workspaces.main.projects]]\nname = \"alpha\"\npath = \"{root}\"\n"
    );
    std::fs::write(tmp.path().join("config.toml"), &config).unwrap();
    let fno_dir = tmp.path().join(".fno");
    std::fs::create_dir_all(&fno_dir).unwrap();
    std::fs::write(fno_dir.join("config.toml"), &config).unwrap();
    std::fs::write(
        tmp.path().join("graph.json"),
        serde_json::to_string(&graph).unwrap(),
    )
    .unwrap();
    let registry_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&registry_dir).unwrap();
    let registry = registry_dir.join("registry.json");
    std::fs::write(&registry, registry_rows.to_string()).unwrap();
    Fixture { tmp, registry }
}

fn crown_registry() -> Value {
    json!({"schema_version": fno_agents::state::REGISTRY_SCHEMA_VERSION, "agents": [
        {"name": "king-a", "status": "live", "crown_scope": "e-1", "crown_level": 2,
         "cwd": "/", "harness": "claude", "created_at": "2026-09-07T00:00:00Z", "log_path": ""},
        {"name": "w-1", "status": "live", "node": "e-1a", "cwd": "/", "harness": "claude",
         "created_at": "2026-09-07T00:00:00Z", "log_path": "", "pid": std::process::id()}
    ]})
}

fn empty_registry() -> Value {
    json!({"schema_version": fno_agents::state::REGISTRY_SCHEMA_VERSION, "agents": []})
}

/// The fixture graph: one epic crown over e-1 (rooted in alpha), ideas inside
/// the scope, one loose idea, one blocked node, one already-fed node.
fn base_graph(plan_path: &Path) -> Value {
    let plan = plan_path.to_string_lossy();
    json!({"entries": [
        {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress", "priority": "p1"},
        {"id": "e-1a", "parent": "e-1", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan},
        {"id": "e-1b", "parent": "e-1", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan},
        {"id": "e-1-fed", "parent": "e-1", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan},
        {"id": "e-loose", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan},
        {"id": "e-blocked", "parent": "e-1", "project": "alpha", "status": "blocked", "priority": "p2", "plan_path": plan}
    ]})
}

/// Canonical JSON text: keys recursively sorted, compact. Both legs pass
/// through this, so key order can never decide a parity verdict.
fn canon(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&map[k]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sorted(v)).unwrap()
}

/// Mask the per-scope digest in worker_name_next (sha1 on the Python leg,
/// sha256 on the port; the digest itself is not the contract).
fn mask_name_next(v: &mut Value) {
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("worker_name_next") {
            obj.insert("worker_name_next".to_string(), json!("<digest>"));
        }
    }
}

/// Run one Python snippet over the fixture and parse its JSON stdout.
/// The Python loader honors FNO_GLOBAL_SETTINGS_PATH for its global tier and
/// `<cwd>/.fno/config.toml` for its cwd tier, so the subprocess runs with cwd
/// pinned to the fixture, the fixture config copied into `.fno/`, and the
/// canonical tier suppressed - its whole config walk lands inside the fixture.
fn py_json(snippet: &str, root: &Path) -> Value {
    let out = Command::new(python_executable())
        .arg("-c")
        .arg(snippet)
        .current_dir(root)
        .env("PYTHONPATH", pythonpath())
        .env("HOME", root) // projects/resolve.py pins $HOME/.fno/config.toml
        .env("FNO_HOME", root)
        .env("FNO_GLOBAL_SETTINGS_PATH", root.join("config.toml"))
        .env("FNO_NO_CANONICAL_CONFIG", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run python leg");
    assert!(
        out.status.success(),
        "python leg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("python leg printed JSON")
}

fn rust_drain(fixture: &Fixture) -> Value {
    let targets =
        fno_agents::active_backlog::native_receipt(fixture.tmp.path(), &fixture.registry)
            .expect("native receipt");
    json!(targets)
}

fn py_drain(root: &Path) -> Value {
    py_json(
        "import json\nfrom fno.active_backlog import drain_targets_as_dicts\nprint(json.dumps(drain_targets_as_dicts(), sort_keys=True))",
        root,
    )
}

fn rust_rows(fixture: &Fixture) -> Value {
    json!(fno_agents::territory::territory_rows(
        fixture.tmp.path(),
        &fixture.registry
    ))
}

fn py_rows(root: &Path) -> Value {
    py_json(
        "import json\nfrom fno.active_backlog import territory_rows\nprint(json.dumps(territory_rows(), sort_keys=True))",
        root,
    )
}

fn rust_feed(fixture: &Fixture, scope: &str) -> Value {
    let mut v =
        fno_agents::territory::blueprint_feed_status(fixture.tmp.path(), &fixture.registry, scope);
    mask_name_next(&mut v);
    v
}

fn py_feed(root: &Path, scope: &str) -> Value {
    let mut v = py_json(
        "import json, os\nfrom fno.worker.blueprint import blueprint_feed\nprint(json.dumps(blueprint_feed(os.environ['FSCOPE']), sort_keys=True))",
        root,
    );
    mask_name_next(&mut v);
    v
}

/// The fixture root moves every run; the goldens must not care. Both legs'
/// output is rendered with the root masked before compare and freeze.
fn canon_masked(v: &Value, root: &Path) -> String {
    canon(v).replace(&root.to_string_lossy().to_string(), "<fixture>")
        .replace(&root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).to_string_lossy().to_string(), "<fixture>")
        // macOS: /var/folders/... is a symlink of /private/var/folders/...
        .replace(
            &format!(
                "/private{}",
                root.to_string_lossy().to_string().trim_start_matches("/private")
            ),
            "<fixture>",
        )
}

fn assert_case(
    label: &str,
    fixture: &Fixture,
    rust: impl FnOnce() -> Value,
    py: impl FnOnce() -> Value,
) -> Value {
    let _env = env_lock();
    // The Rust leg runs in-process: FNO_CONFIG is its sole config candidate,
    // so pinning it here pins the whole config walk to the fixture. Without
    // it the walk leaks to the canonical + global config and the comparison
    // silently reads two different worlds.
    std::env::set_var("FNO_CONFIG", fixture.tmp.path().join("config.toml"));
    std::env::set_var("FNO_HOME", fixture.tmp.path());
    let rust = rust();
    let golden = Golden {
        exit: Some(0),
        streams: vec![canon_masked(&rust, fixture.tmp.path())],
    };
    let oracle = capture_mode().then(|| {
        if !python_available() {
            panic!("FNO_CAPTURE_GOLDEN=1 needs the Python leg importable");
        }
        let py = py();
        // Structural compare: JSON object key order must never decide a
        // parity verdict, and Value equality is order-independent.
        assert_eq!(rust, py, "rust leg differs from the python leg");
        Golden {
            exit: Some(0),
            streams: vec![canon_masked(&py, fixture.tmp.path())],
        }
    });
    assert_golden_common("territory", label, &golden, oracle);
    rust
}

#[test]
fn drain_receipt_matches_python_two_territories() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), crown_registry());
    assert_case(
        "drain_two_territories",
        &fixture,
        || rust_drain(&fixture),
        || py_drain(fixture.tmp.path()),
    );
}

#[test]
fn drain_receipt_matches_python_kingless_only() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), empty_registry());
    assert_case(
        "drain_kingless_only",
        &fixture,
        || rust_drain(&fixture),
        || py_drain(fixture.tmp.path()),
    );
}

#[test]
fn drain_receipt_matches_python_per_project_disabled() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture(
        "enabled = { alpha = false }\n",
        base_graph(&plan_doc),
        crown_registry(),
    );
    assert_case(
        "drain_per_project_disabled",
        &fixture,
        || rust_drain(&fixture),
        || py_drain(fixture.tmp.path()),
    );
}

#[test]
fn rows_projection_matches_python() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), crown_registry());
    assert_case(
        "rows_projection",
        &fixture,
        || rust_rows(&fixture),
        || py_rows(fixture.tmp.path()),
    );
}

#[test]
fn feed_status_matches_python() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n---").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), crown_registry());
    let mut rec = fno_agents::territory::read_record(fixture.tmp.path(), "e-1");
    rec["fed"]["e-1-fed"] = json!({"at": "2020-01-01T00:00:00Z", "ok": false});
    fno_agents::territory::write_record(fixture.tmp.path(), "e-1", &mut rec);
    assert_case(
        "feed_status",
        &fixture,
        || rust_feed(&fixture, "e-1"),
        || {
            std::env::set_var("FSCOPE", "e-1");
            py_feed(fixture.tmp.path(), "e-1")
        },
    );
}
