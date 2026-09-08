//! parity-stage: characterization
//! parity-oracle: (none - the Python legs were deleted in the same change; the goldens are the contract)
//!
//! Characterization for the territory port (x-e221): the Rust fact set
//! (`active_backlog::native_receipt` / `territory::territory_rows` /
//! `blueprint_feed_status`) is pinned by the frozen goldens under
//! tests/golden/territory/. The goldens were captured from the Python legs
//! while a differential oracle still ran (step 2 of the port protocol,
//! docs/architecture/dual-implementation-inventory.md); that oracle is gone,
//! and capture mode now refuses - a golden can only be captured while the old
//! leg runs.
//!
//! One field is normalized before compare: the feed receipt's
//! worker_name_next embeds a label with a per-scope digest (the deleted
//! Python leg used sha1, the port sha256; only per-scope stability is
//! load-bearing, so the digest is masked, never compared).

use common::{assert_golden as assert_golden_common, capture_mode, Golden};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

mod common;

/// Serialize FNO_CONFIG/FNO_HOME mutation across the parallel test threads.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// -------------------------------------------------------------------------
// The shared fixture: one tempdir holding config.toml (state_dir pointed
// back into it), graph.json, and the agents registry the fact set reads.
// -------------------------------------------------------------------------

struct Fixture {
    tmp: tempfile::TempDir,
    registry: PathBuf,
}

fn build_fixture(active_backlog_extra: &str, graph: Value, registry_rows: Value) -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_string_lossy().replace('\'', "");
    // state_dir points into the fixture so the fact set resolves the graph,
    // the registry, and the blueprinter records inside it.
    // active_backlog_extra folds into the same table (never a second header,
    // TOML forbids it).
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

/// Canonical JSON text: keys recursively sorted, compact, so key order can
/// never decide a verdict.
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

/// Mask the per-scope digest in worker_name_next (the deleted Python leg used
/// sha1, the port sha256; the digest itself is not the contract).
fn mask_name_next(v: &mut Value) {
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("worker_name_next") {
            obj.insert("worker_name_next".to_string(), json!("<digest>"));
        }
    }
}

fn rust_drain(fixture: &Fixture) -> Value {
    let targets = fno_agents::active_backlog::native_receipt(fixture.tmp.path(), &fixture.registry)
        .expect("native receipt");
    json!(targets)
}

fn rust_rows(fixture: &Fixture) -> Value {
    json!(fno_agents::territory::territory_rows(
        fixture.tmp.path(),
        &fixture.registry
    ))
}

fn rust_feed(fixture: &Fixture, scope: &str) -> Value {
    let mut v =
        fno_agents::territory::blueprint_feed_status(fixture.tmp.path(), &fixture.registry, scope);
    mask_name_next(&mut v);
    v
}

/// The fixture root moves every run; the goldens must not care. Output is
/// rendered with the root (and its /private symlink form on macOS) masked
/// before compare and freeze.
fn canon_masked(v: &Value, root: &Path) -> String {
    canon(v)
        .replace(&root.to_string_lossy().to_string(), "<fixture>")
        .replace(
            &root
                .canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .to_string(),
            "<fixture>",
        )
        .replace(
            &format!(
                "/private{}",
                root.to_string_lossy()
                    .to_string()
                    .trim_start_matches("/private")
            ),
            "<fixture>",
        )
}

fn assert_case(label: &str, fixture: &Fixture, rust: impl FnOnce() -> Value) -> Value {
    let _env = env_lock();
    // FNO_CONFIG is the sole config candidate, so pinning it here pins the
    // whole config walk to the fixture. Without it the walk leaks to the
    // canonical + global config and the golden quietly describes the
    // operator's machine.
    std::env::set_var("FNO_CONFIG", fixture.tmp.path().join("config.toml"));
    std::env::set_var("FNO_HOME", fixture.tmp.path());
    let rust = rust();
    let golden = Golden {
        exit: Some(0),
        streams: vec![canon_masked(&rust, fixture.tmp.path())],
    };
    if capture_mode() {
        panic!(
            "[{label}] FNO_CAPTURE_GOLDEN=1 but the Python oracle is deleted; \
             the goldens on disk are the contract"
        );
    }
    assert_golden_common("territory", label, &golden, None);
    rust
}

#[test]
fn drain_receipt_two_territories() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), crown_registry());
    assert_case("drain_two_territories", &fixture, || rust_drain(&fixture));
}

#[test]
fn drain_receipt_kingless_only() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), empty_registry());
    assert_case("drain_kingless_only", &fixture, || rust_drain(&fixture));
}

#[test]
fn drain_receipt_per_project_disabled() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture(
        "enabled = { alpha = false }\n",
        base_graph(&plan_doc),
        crown_registry(),
    );
    assert_case("drain_per_project_disabled", &fixture, || {
        rust_drain(&fixture)
    });
}

#[test]
fn rows_projection() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), crown_registry());
    assert_case("rows_projection", &fixture, || rust_rows(&fixture));
}

#[test]
fn feed_status() {
    let plan = tempfile::TempDir::new().unwrap();
    let plan_doc = plan.path().join("idea-plan.md");
    std::fs::write(&plan_doc, "---\nstatus: design\n---\n---").unwrap();
    let fixture = build_fixture("", base_graph(&plan_doc), crown_registry());
    let mut rec = fno_agents::territory::read_record(fixture.tmp.path(), "e-1");
    rec["fed"]["e-1-fed"] = json!({"at": "2020-01-01T00:00:00Z", "ok": false});
    fno_agents::territory::write_record(fixture.tmp.path(), "e-1", &mut rec);
    assert_case("feed_status", &fixture, || rust_feed(&fixture, "e-1"));
}
