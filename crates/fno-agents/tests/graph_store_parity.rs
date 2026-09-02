//! Differential parity for the graph store port (protocol steps 1-2,
//! docs/architecture/dual-implementation-inventory.md). The Rust leg is
//! `fno_agents::graph_store` + the keeper's typed ops; the Python leg is the
//! file-reading store in `cli/src/fno/graph/store.py` until the deletion
//! half of this port lands. Both run over identical fixtures and must agree
//! byte-for-byte on the read serialization, the per-step results, and the
//! published file bytes.
//!
//! The only bytes allowed to differ are the now()-stamps both legs write
//! (touched_at, deferred_at); they are normalized symmetrically before the
//! comparison and their PRESENCE is still asserted by the pipeline steps
//! that write them.
//!
//! `FNO_CAPTURE_GOLDEN=1`: the helper runs the Python leg, asserts
//! Rust==Python, and writes the Python output as the golden this test
//! freezes against when the Python leg is deleted (step 4 converts this
//! file to a characterization test).

//! parity-stage: differential
//! parity-oracle: fno.graph.store

use base64::Engine as _;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

/// The interpreter that can import the store's dependencies (pydantic at
/// minimum): the project venv when present, else `uv run` against the cli
/// project, which resolves the env itself.
fn python_command() -> (String, Vec<String>) {
    let venv = pythonpath().join("../.venv/bin/python");
    if venv.is_file() {
        return (venv.display().to_string(), Vec::new());
    }
    let cli = pythonpath().parent().unwrap().to_path_buf();
    (
        "uv".to_string(),
        vec![
            "run".into(),
            "--project".into(),
            cli.display().to_string(),
            "python".into(),
        ],
    )
}

fn python_available() -> bool {
    let (bin, pre) = python_command();
    let mut cmd = Command::new(bin);
    cmd.args(pre)
        .arg("-c")
        .arg("import fno.graph.store")
        .env("PYTHONPATH", pythonpath());
    matches!(cmd.output(), Ok(o) if o.status.success())
}

/// Run the PYTHON leg over one fixture + op list. Prints one JSON object:
/// the read serialization, per-step results, and the final file bytes.
fn python_probe(graph: &Path, ops: &serde_json::Value) -> serde_json::Value {
    let code = r#"
import base64, json, os, sys
from pathlib import Path
from fno.graph import store

graph = Path(os.environ["GRAPH"])
ops = json.loads(os.environ["OPS"])
out = {"steps": []}

read = store.read_graph(graph)
out["read"] = json.dumps(read, indent=2, ensure_ascii=True)

try:
    strict = store.read_graph_strict(graph)
    out["strict"] = json.dumps(strict, indent=2, ensure_ascii=True)
except Exception as exc:  # noqa: BLE001 - the error KIND is the parity datum
    out["strict_error"] = type(exc).__name__

def find(entries, node_id):
    for e in entries:
        if e.get("id") == node_id:
            return e
    return None

for op in ops:
    name = op["name"]
    if name == "set_field":
        def mutator(entries, op=op):
            node = find(entries, op["node_id"])
            if node is not None:
                node[op["field"]] = op["value"]
            return entries
        store.locked_mutate_graph(graph, mutator)
    elif name == "new_node":
        def mutator(entries, op=op):
            entries.append(dict(op["entry"]))
            return entries
        store.locked_mutate_graph(graph, mutator)
    elif name == "append_progress_note":
        found, plan_path = store.append_progress_note(graph, op["node_id"], op["note"])
        out["steps"].append({"append_progress_note": [found, plan_path]})
    elif name == "append_encounter":
        appended, error, reason = store.append_encounter(graph, op["node_id"], op["record"])
        out["steps"].append({"append_encounter": [appended, error, reason]})
    elif name == "append_wave_note":
        found, error = store.append_wave_note(graph, op["node_id"], op["note"])
        out["steps"].append({"append_wave_note": [found, error]})
    elif name == "session_append":
        orig = store._observe_model
        store._observe_model = lambda h, s: op["observed"]
        try:
            found, added = store.append_session_record(
                graph, op["node_id"], phase=op["phase"], harness=op["harness"],
                session_id=op["session_id"], effort=op.get("effort"),
                started_at=op.get("started_at"), ended_at=op.get("ended_at"))
        finally:
            store._observe_model = orig
        out["steps"].append({"session_append": [found, added]})
    elif name == "session_remove_open":
        found, removed = store.remove_open_session_record(
            graph, op["node_id"], phase=op["phase"], harness=op["harness"],
            session_id=op["session_id"], started_at=op["started_at"])
        out["steps"].append({"session_remove_open": [found, removed]})
    elif name == "session_reap_open":
        report = store.reap_open_session_record(
            graph, op["node_id"], phase=op["phase"], harness=op["harness"],
            session_id=op["session_id"], ended_at=op.get("ended_at"))
        report.pop("status_after", None)
        report.pop("remaining_open_do", None)
        out["steps"].append({"session_reap_open": report})
    elif name == "set_related":
        def mutator(entries, op=op):
            store.set_related(entries, op["node_id"], op["desired"])
            return entries
        store.locked_mutate_graph(graph, mutator)
    elif name == "read_after":
        out["steps"].append({"read_after": json.dumps(store.read_graph(graph), indent=2, ensure_ascii=True)})

out["file"] = base64.b64encode(graph.read_bytes()).decode()
print(json.dumps(out))
"#;
    let (bin, pre) = python_command();
    let out = Command::new(bin)
        .args(pre)
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .env("GRAPH", graph)
        .env("OPS", serde_json::to_string(ops).unwrap())
        .output()
        .expect("run python store probe");
    assert!(
        out.status.success(),
        "python store probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("probe output is one JSON object")
}

/// Run the RUST leg over the same fixture + ops: read, then each step
/// through the exact functions the keeper serves (typed ops via
/// `apply_op_for_tests`, plain client mutators via `locked_mutate`).
fn rust_probe(graph: &Path, ops: &serde_json::Value) -> serde_json::Value {
    use fno_agents::graph_keeper::apply_op_for_tests;
    use fno_agents::graph_store::{self, MutateInput};

    let mutate = |entries: Vec<Value>, g: &Path| -> graph_store::MutateOutcome {
        // Single-writer probes: no interleaving, so the snapshot is current.
        let base = graph_store::file_content_version(g);
        graph_store::locked_mutate(
            g,
            MutateInput {
                entries,
                canonical_path: None,
                base_version: Some(base),
            },
            std::time::Duration::from_secs(5),
        )
        .expect("rust locked_mutate")
    };

    // The soft read swallows corruption to [] (read_graph's contract); the
    // strict read surfaces the error kind.
    let soft = |g: &Path| -> Result<Vec<Value>, graph_store::StoreError> {
        match graph_store::read_defaulted(g, false) {
            Ok(v) => Ok(v),
            Err(
                e @ graph_store::StoreError::Corrupt(_)
                | e @ graph_store::StoreError::MalformedRoot(_)
                | e @ graph_store::StoreError::Unreadable(_, _),
            ) => {
                eprintln!("{e}");
                Ok(vec![])
            }
            Err(e) => Err(e),
        }
    };

    let read_now = |g: &Path| -> String {
        let entries = soft(g).expect("rust read");
        graph_store::serialize_entries(&entries)
    };

    let mut steps: Vec<Value> = Vec::new();
    let mut json_out = serde_json::Map::new();
    json_out.insert("read".into(), Value::String(read_now(graph)));

    match graph_store::read_defaulted(graph, false) {
        Ok(strict) => {
            json_out.insert(
                "strict".into(),
                Value::String(graph_store::serialize_entries(&strict)),
            );
        }
        Err(e) => {
            json_out.insert("strict_error".into(), Value::String(strict_error_kind(&e)));
        }
    }

    for op in ops.as_array().expect("ops is a list") {
        let name = op.get("name").and_then(Value::as_str).unwrap_or("");
        match name {
            "set_field" => {
                let mut entries = graph_store::read_defaulted(graph, false).unwrap();
                let node = op.get("node_id").and_then(Value::as_str).unwrap_or("");
                let field = op.get("field").and_then(Value::as_str).unwrap_or("");
                let value = op.get("value").cloned().unwrap_or(Value::Null);
                if let Some(e) = entries
                    .iter_mut()
                    .find(|e| graph_store::entry_id(e) == Some(node))
                {
                    e.as_object_mut().unwrap().insert(field.to_string(), value);
                }
                mutate(entries, graph);
            }
            "new_node" => {
                let mut entries = graph_store::read_defaulted(graph, false).unwrap();
                entries.push(op.get("entry").cloned().unwrap_or(Value::Null));
                mutate(entries, graph);
            }
            "set_related" => {
                let mut entries = graph_store::read_defaulted(graph, false).unwrap();
                let node = op.get("node_id").and_then(Value::as_str).unwrap_or("");
                let desired: Vec<String> = op
                    .get("desired")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut request = serde_json::Map::new();
                request.insert("name".into(), Value::String("set_related".into()));
                request.insert(
                    "params".into(),
                    serde_json::json!({"node_id": node, "desired": desired}),
                );
                apply_op_for_tests(&mut entries, &Value::Object(request))
                    .expect("set_related on parity fixture");
                mutate(entries, graph);
            }
            "read_after" => {
                steps.push(serde_json::json!({"read_after": read_now(graph)}));
            }
            other => {
                let mut entries = graph_store::read_defaulted(graph, false).unwrap();
                let mut request = serde_json::Map::new();
                request.insert("name".into(), Value::String(other.to_string()));
                request.insert("params".into(), op.clone());
                let op_result =
                    apply_op_for_tests(&mut entries, &Value::Object(request)).unwrap_or_else(
                        |e| panic!("rust op {other}: {e}"),
                    );
                mutate(entries, graph);
                let shaped = match other {
                    "append_progress_note" => serde_json::json!([
                        op_result.get("found"), op_result.get("plan_path")]),
                    "append_encounter" => serde_json::json!([
                        op_result.get("appended"), op_result.get("error"),
                        op_result.get("reason")]),
                    "append_wave_note" => serde_json::json!([
                        op_result.get("found"), op_result.get("error")]),
                    "session_append" => serde_json::json!([
                        op_result.get("found"), op_result.get("added")]),
                    "session_remove_open" => serde_json::json!([
                        op_result.get("found"), op_result.get("removed")]),
                    "session_reap_open" => {
                        let mut r = op_result.clone();
                        let o = r.as_object_mut().unwrap();
                        o.shift_remove("status_after");
                        o.shift_remove("remaining_open_do");
                        r
                    }
                    _ => op_result,
                };
                steps.push(serde_json::json!({other.to_string(): shaped}));
            }
        }
    }

    json_out.insert("steps".into(), Value::Array(steps));
    let file = std::fs::read(graph).expect("rust published file");
    json_out.insert(
        "file".into(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(&file)),
    );
    Value::Object(json_out)
}

/// Textual normalization for the two stamp shapes at every JSON escaping
/// layer (file bytes carry one layer; step strings two).
fn normalize_bytes(text: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(\\)?"(touched_at|deferred_at)(\\)?"(\\)?: (\\)?"[^"\\\\]*"#)
            .unwrap()
    });
    re.replace_all(text, |caps: &regex::Captures| {
        let esc = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        format!("{esc}\"{}{esc}\"{esc}: {esc}\"<TS>", &caps[2])
    })
    .into_owned()
}


/// On divergence, dump both sides to files a human (or a script) can diff,
/// then panic. The assert message alone wraps values in escaping layers.
fn assert_dumped(name: &str, surface: &str, rust_v: &Value, py_v: &Value) {
    if rust_v != py_v {
        let dir = std::env::temp_dir();
        let r = dir.join(format!("parity-fail-{name}-{surface}.rust.json"));
        let p = dir.join(format!("parity-fail-{name}-{surface}.python.json"));
        std::fs::write(&r, serde_json::to_vec_pretty(rust_v).unwrap()).unwrap();
        std::fs::write(&p, serde_json::to_vec_pretty(py_v).unwrap()).unwrap();
        panic!(
            "{name}: {surface} diverged; dumped {} and {}",
            r.display(),
            p.display()
        );
    }
}

fn strict_error_kind(e: &fno_agents::graph_store::StoreError) -> String {
    use fno_agents::graph_store::StoreError as E;
    match e {
        E::Corrupt(_)
        | E::Unreadable(_, _)
        | E::EmptyFieldUpdate(_)
        | E::Invalid(_)
        | E::Io(_)
        | E::LockTimeout(_, _)
        | E::Conflict => "GraphUnreadableError".to_string(),
        E::MalformedRoot(_) => "GraphMalformedRootError".to_string(),
    }
}

/// The two volatile now()-stamps both legs write, normalized symmetrically:
/// a recursive walk over the parsed values, so escaping layers cannot hide a
/// stamp from the normalizer.
fn normalize_volatile(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if (k == "touched_at" || k == "deferred_at") && val.is_string() {
                    out.insert(k.clone(), Value::String("<TS>".into()));
                } else {
                    out.insert(k.clone(), normalize_volatile(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_volatile).collect()),
        // A step value that IS a serialized JSON payload (read_after) is
        // parsed and normalized structurally, so stamps inside it cannot
        // hide and formatting cannot leak into the comparison.
        Value::String(text) if text.trim_start().starts_with('[') || text.trim_start().starts_with('{') => {
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) => normalize_volatile(&parsed),
                Err(_) => Value::String(text.clone()),
            }
        }
        other => other.clone(),
    }
}

// -------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------

fn fixture_basic() -> String {
    r#"{
  "entries": [
    {
      "id": "ab-0001",
      "slug": "alpha-node",
      "title": "Alpha node",
      "priority": "p1",
      "type": "feature",
      "status": "ready",
      "plan_path": null,
      "details": "alpha details",
      "created_at": "2026-08-01T00:00:00+00:00",
      "progress_notes": [
        {"ts": "2026-08-01T01:00:00Z", "text": "earlier"}
      ]
    },
    {
      "id": "ab-0002",
      "title": "Beta node",
      "priority": "high",
      "parent": "ab-0001",
      "blocked_by": ["ab-0009"],
      "created_at": "2026-08-02T00:00:00+00:00"
    },
    {
      "id": "ab-0003",
      "title": "Gamma legacy",
      "_status": "claimed",
      "session_id": "sess-legacy-1",
      "completed_at": "deferred:2026-07-01T00:00:00+00:00"
    },
    "junk-string-row",
    42,
    null
  ]
}
"#
    .to_string()
}

fn fixture_unicode_and_exotics() -> String {
    let mut e = serde_json::Map::new();
    e.insert("id".into(), Value::String("ab-00ff".into()));
    e.insert(
        "title".into(),
        Value::String("h\u{e9}llo \u{1f600} w\u{f6}rld".into()),
    );
    e.insert("rank".into(), serde_json::json!(1.5));
    e.insert(
        "details".into(),
        Value::String("line\nbreak\ttab\"quote\"\\slash\u{7f}".into()),
    );
    e.insert("related".into(), serde_json::json!(["ab-0001"]));
    // The fixture file is written exactly the way the store publishes a
    // one-entry graph, so the two legs start from identical bytes.
    fno_agents::graph_store::serialize_graph_file(&[Value::Object(e)])
}

// -------------------------------------------------------------------------
// The parity cases
// -------------------------------------------------------------------------

/// One differential case: fixture -> ops -> compare every surface.
fn run_case(name: &str, fixture: String, ops: serde_json::Value) {
    let py_dir = tempfile::tempdir().expect("py dir");
    let rs_dir = tempfile::tempdir().expect("rs dir");
    let py_graph = py_dir.path().join("graph.json");
    let rs_graph = rs_dir.path().join("graph.json");
    std::fs::write(&py_graph, &fixture).unwrap();
    std::fs::write(&rs_graph, &fixture).unwrap();

    let py = python_probe(&py_graph, &ops);
    let rs = rust_probe(&rs_graph, &ops);

    let py_read = py.get("read").and_then(Value::as_str).unwrap_or("");
    let rs_read = rs.get("read").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        rs_read, py_read,
        "{name}: the defaulted read must be byte-identical\n--- rust ---\n{rs_read}\n--- python ---\n{py_read}"
    );

    let py_strict = py.get("strict").and_then(Value::as_str);
    let rs_strict = rs.get("strict").and_then(Value::as_str);
    assert_eq!(rs_strict, py_strict, "{name}: strict read must match");
    assert_eq!(
        rs.get("strict_error"),
        py.get("strict_error"),
        "{name}: strict-read error kind must match"
    );

    let py_steps = normalize_volatile(py.get("steps").unwrap_or(&Value::Null));
    let rs_steps = normalize_volatile(rs.get("steps").unwrap_or(&Value::Null));
    assert_dumped(name, "steps", &rs_steps, &py_steps);

    let py_file = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(py.get("file").and_then(Value::as_str).unwrap_or(""))
            .expect("py file b64"),
    )
    .expect("py file bytes are utf8");
    let rs_file = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(rs.get("file").and_then(Value::as_str).unwrap_or(""))
            .expect("rs file b64"),
    )
    .expect("rs file bytes are utf8");
    let py_root: Value = serde_json::from_str(&py_file).expect("py file parses");
    let rs_root: Value = serde_json::from_str(&rs_file).expect("rs file parses");
    // Byte identity, checked structurally then textually: the structural
    // compare localizes a divergence; the textual one on normalized bytes
    // catches ordering the structural compare forgives.
    let norm_rs = normalize_volatile(&rs_root);
    let norm_py = normalize_volatile(&py_root);
    assert_dumped(name, "file-content", &norm_rs, &norm_py);
    assert_eq!(
        normalize_bytes(&rs_file), normalize_bytes(&py_file),
        "{name}: the published file must be byte-identical modulo volatile stamps\n--- rust ---\n{rs_file}\n--- python ---\n{py_file}"
    );

    if std::env::var("FNO_CAPTURE_GOLDEN").as_deref() == Ok("1") {
        capture_golden(name, &py);
    }
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/graph_store")
}

fn capture_golden(name: &str, py: &serde_json::Value) {
    let dir = golden_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(py).unwrap()).unwrap();
    println!("captured golden {}", path.display());
}

#[test]
fn differential_reads_and_mutations_match_the_python_leg() {
    if !python_available() {
        eprintln!("skip: the fno Python package is unavailable");
        return;
    }
    run_case(
        "defaults_and_lifecycle",
        fixture_basic(),
        serde_json::json!([
            {"name": "set_field", "node_id": "ab-0001", "field": "priority", "value": "p0"},
            {"name": "append_progress_note", "node_id": "ab-0001",
             "note": {"ts": "2026-08-02T02:00:00Z", "text": "second note"}},
            {"name": "append_encounter", "node_id": "ab-0001",
             "record": {"ts": "2026-08-02T03:00:00Z", "session_id": "voter-1", "harness": "claude",
                        "evidence": "cost me a rebase"}},
            {"name": "append_wave_note", "node_id": "ab-0002",
             "note": {"wave": "1", "text": "wave note"}},
            {"name": "new_node",
             "entry": {"id": "ab-0004", "title": "Delta node the port", "priority": "p2"}},
            {"name": "session_append", "node_id": "ab-0004", "phase": "do", "harness": "claude",
             "session_id": "0123abcd-0000-0000-0000-000000000004",
             "started_at": "2026-08-02T04:00:00Z",
             "observed": {"kind": "observed", "model": "test-model", "samples": 3}},
            {"name": "session_append", "node_id": "ab-0004", "phase": "do", "harness": "claude",
             "session_id": "0123abcd-0000-0000-0000-000000000004",
             "started_at": "2026-08-02T04:00:00Z", "ended_at": "2026-08-02T05:00:00Z",
             "observed": {"kind": "observed", "model": "test-model", "samples": 5}},
            {"name": "read_after"}
        ]),
    );
}

#[test]
fn differential_legacy_rows_and_defer_backfill_match() {
    if !python_available() {
        eprintln!("skip: the fno Python package is unavailable");
        return;
    }
    run_case(
        "legacy_backfill",
        fixture_basic(),
        serde_json::json!([
            {"name": "read_after"},
            {"name": "set_field", "node_id": "ab-0003", "field": "priority", "value": "p3"}
        ]),
    );
}

#[test]
fn differential_unicode_and_escapes_are_byte_identical() {
    if !python_available() {
        eprintln!("skip: the fno Python package is unavailable");
        return;
    }
    run_case(
        "unicode_exotics",
        fixture_unicode_and_exotics(),
        serde_json::json!([
            {"name": "set_field", "node_id": "ab-00ff", "field": "priority", "value": "p1"},
            {"name": "read_after"}
        ]),
    );
}

#[test]
fn differential_session_lifecycle_matches() {
    if !python_available() {
        eprintln!("skip: the fno Python package is unavailable");
        return;
    }
    let entries = serde_json::json!([{"id": "ab-00aa", "title": "Session host"}]);
    let fixture = format!(
        "{{\n  \"entries\": {}\n}}\n",
        serde_json::to_string_pretty(&entries).unwrap()
    );
    run_case(
        "session_lifecycle",
        fixture,
        serde_json::json!([
            {"name": "session_append", "node_id": "ab-00aa", "phase": "do", "harness": "claude",
             "session_id": "0123abcd-0000-0000-0000-00000000aaaa",
             "started_at": "2026-08-02T04:00:00Z",
             "observed": {"kind": "no-transcript"}},
            {"name": "session_append", "node_id": "ab-00aa", "phase": "do", "harness": "claude",
             "session_id": "0123abcd-0000-0000-0000-00000000aaaa",
             "started_at": "2026-08-02T04:00:00Z", "ended_at": "2026-08-02T05:30:00Z",
             "observed": {"kind": "observed", "model": "later-model", "samples": 9}},
            {"name": "session_append", "node_id": "ab-00aa", "phase": "review", "harness": "codex",
             "session_id": "0123abcd-0000-0000-0000-00000000bbbb",
             "started_at": "2026-08-02T06:00:00Z",
             "observed": {"kind": "not-file-backed"}},
            {"name": "session_remove_open", "node_id": "ab-00aa", "phase": "review",
             "harness": "codex", "session_id": "0123abcd-0000-0000-0000-00000000bbbb",
             "started_at": "2026-08-02T06:00:00Z"},
            {"name": "session_append", "node_id": "ab-00aa", "phase": "review", "harness": "codex",
             "session_id": "0123abcd-0000-0000-0000-00000000bbbb",
             "started_at": "2026-08-02T06:00:00Z",
             "observed": {"kind": "not-file-backed"}},
            {"name": "session_reap_open", "node_id": "ab-00aa", "phase": "review",
             "harness": "codex", "session_id": "0123abcd-0000-0000-0000-00000000bbbb",
             "ended_at": "2026-08-02T07:00:00Z"},
            {"name": "read_after"}
        ]),
    );
}

#[test]
fn differential_related_mirror_matches() {
    if !python_available() {
        eprintln!("skip: the fno Python package is unavailable");
        return;
    }
    let fixture = r#"{
  "entries": [
    {"id": "ab-00c1", "title": "C1"},
    {"id": "ab-00c2", "title": "C2"},
    {"id": "ab-00c3", "title": "C3"}
  ]
}
"#
    .to_string();
    run_case(
        "related_mirror",
        fixture,
        serde_json::json!([
            {"name": "set_related", "node_id": "ab-00c1", "desired": ["ab-00c2", "ab-00c3"]},
            {"name": "set_related", "node_id": "ab-00c1", "desired": ["ab-00c3"]},
            {"name": "read_after"}
        ]),
    );
}

#[test]
fn differential_corrupt_and_malformed_roots_agree_on_kinds() {
    if !python_available() {
        eprintln!("skip: the fno Python package is unavailable");
        return;
    }
    // The soft read swallows corruption to []; the strict read raises, and
    // the RAISED KIND is the parity datum. Python's _read_json copies the
    // unreadable bytes to a .json.bak on the soft path.
    for (name, body, want_kind) in [
        ("corrupt", "{not json", "GraphUnreadableError"),
        ("malformed_root", "{\"no_entries\": []}", "GraphMalformedRootError"),
        ("entries_not_list", "{\"entries\": \"x\"}", "GraphUnreadableError"),
        ("empty_file", "", "GraphUnreadableError"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph.json");
        std::fs::write(&graph, body).unwrap();
        let ops = serde_json::json!([]);
        let py = python_probe(&graph, &ops);
        let rs = rust_probe(&graph, &ops);
        assert_eq!(
            py.get("read").and_then(Value::as_str),
            Some("[]"),
            "{name}: soft read swallows"
        );
        assert_eq!(
            rs.get("read").and_then(Value::as_str),
            Some("[]"),
            "{name}: rust soft read swallows"
        );
        assert_eq!(
            py.get("strict_error").and_then(Value::as_str),
            Some(want_kind),
            "{name}: python strict kind"
        );
        assert_eq!(
            rs.get("strict_error").and_then(Value::as_str),
            py.get("strict_error").and_then(Value::as_str),
            "{name}: strict kinds must agree"
        );
    }
}

#[test]
fn concurrent_writers_never_lose_an_update_through_the_bounded_cycle() {
    // The mutation-under-concurrency property the protocol demands: eight
    // writers each appending a distinct node through the locked cycle. The
    // bounded lock serializes the critical sections, so every completed
    // publish survives and the final file parses to the union.
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    std::fs::write(&graph, "{\n  \"entries\": []\n}\n").unwrap();
    use fno_agents::graph_store::{self, MutateInput};
    let graph_for_threads = graph.clone();
    let mut handles = Vec::new();
    for i in 0..8 {
        let g = graph_for_threads.clone();
        handles.push(std::thread::spawn(move || {
            // The snapshot read runs outside the lock; a stale snapshot
            // answers Conflict and the cycle retries until it lands.
            loop {
                let base = graph_store::file_content_version(&g);
                let mut entries = graph_store::read_defaulted(&g, false).unwrap();
                entries.push(serde_json::json!({
                    "id": format!("ab-c0nc{i:04}"),
                    "title": format!("concurrent {i}")
                }));
                match graph_store::locked_mutate(
                    &g,
                    MutateInput {
                        entries,
                        canonical_path: None,
                        base_version: Some(base),
                    },
                    std::time::Duration::from_secs(10),
                ) {
                    Ok(outcome) => return Ok(outcome),
                    Err(graph_store::StoreError::Conflict) => continue,
                    Err(e) => return Err(e),
                }
            }
        }));
    }
    let mut landed = 0;
    for h in handles {
        if h.join().unwrap().is_ok() {
            landed += 1;
        }
    }
    assert_eq!(landed, 8, "every retried writer eventually lands");
    assert!(landed >= 1, "at least one concurrent writer must land");
    let final_entries = graph_store::read_defaulted(&graph, false).unwrap();
    let ids: Vec<String> = final_entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert_eq!(
        ids.len(),
        landed,
        "no lost updates: every landed publish's node is in the final file"
    );
    // The sidecar matches the published bytes after the storm.
    let body = std::fs::read_to_string(&graph).unwrap();
    let sidecar = std::fs::read_to_string(format!("{}.sha256", graph.display())).unwrap();
    let digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        format!("{:x}\n", h.finalize())
    };
    assert_eq!(sidecar, digest, "the sidecar matches the published bytes");
}
