//! Characterization for the graph store port (protocol steps 3-4,
//! docs/architecture/dual-implementation-inventory.md). The Rust leg is
//! `fno_agents::graph_store` + the keeper's typed ops; the Python leg it is
//! frozen against was the file-reading store in `cli/src/fno/graph/store.py`,
//! deleted in the same change that flipped this file. The goldens in
//! `tests/golden/graph_store/` were captured from the PYTHON leg while both
//! legs lived: the differential stage asserted Rust==Python byte-for-byte
//! over these exact fixtures and op sequences, then froze Python's output.
//!
//! The only bytes allowed to differ from a golden are the now()-stamps the
//! pipeline writes (touched_at, deferred_at); they are normalized before the
//! comparison and their PRESENCE is still asserted by the pipeline steps
//! that write them. Two behavioral cases (error kinds, concurrent writers)
//! never had a byte-parity surface and stay as direct Rust assertions.
//!
//! The oracle is the flock helper the deletion retired: the symbol is the
//! identity of the leg, and the provenance gate asserts it is GONE.

//! parity-stage: characterization
//! parity-oracle: fno.graph.store._acquire_flock

use base64::Engine as _;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// The two volatile now()-stamps the pipeline writes, normalized before any
/// comparison: a recursive walk over the parsed values, so escaping layers
/// cannot hide a stamp from the normalizer.
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

/// On divergence, dump the live output and the frozen golden to files a
/// human (or a script) can diff, then panic. The assert message alone wraps
/// values in escaping layers.
fn assert_frozen(name: &str, surface: &str, live: &Value, golden: &Value) {
    if live != golden {
        let dir = std::env::temp_dir();
        let r = dir.join(format!("parity-fail-{name}-{surface}.live.json"));
        let p = dir.join(format!("parity-fail-{name}-{surface}.golden.json"));
        std::fs::write(&r, serde_json::to_vec_pretty(live).unwrap()).unwrap();
        std::fs::write(&p, serde_json::to_vec_pretty(golden).unwrap()).unwrap();
        panic!(
            "{name}: {surface} diverged from the golden; dumped {} and {}",
            r.display(),
            p.display()
        );
    }
}

/// Run the RUST leg over the fixture + ops: read, then each step through the
/// exact functions the keeper serves (typed ops via `apply_op_for_tests`,
/// plain client mutators via `locked_mutate`). Returns the probe object the
/// goldens were captured in the shape of.
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

    // The STRICT probe: backup_on_corrupt=false is read_graph_strict's
    // read-only diagnosis contract. The soft variant would swallow a
    // malformed root to [] and the taxonomy arm below would see no error at
    // all.
    match graph_store::read_defaulted_opts(graph, false, false) {
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

// -------------------------------------------------------------------------
// Fixtures (identical bytes to what the differential stage ran)
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
    // one-entry graph, so the frozen comparison starts from identical bytes.
    fno_agents::graph_store::serialize_graph_file(&[Value::Object(e)])
}

// -------------------------------------------------------------------------
// The characterization cases
// -------------------------------------------------------------------------

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/graph_store")
}

/// One characterization case: fixture -> ops -> the live Rust output must
/// equal the frozen Python golden on every surface, modulo volatile stamps.
fn run_case(name: &str, fixture: String, ops: serde_json::Value) {
    let golden_path = golden_dir().join(format!("{name}.json"));
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|e| panic!("golden {}: {e}", golden_path.display())),
    )
    .expect("golden parses");

    let dir = tempfile::tempdir().expect("rs dir");
    let graph = dir.path().join("graph.json");
    std::fs::write(&graph, &fixture).unwrap();
    let rs = rust_probe(&graph, &ops);

    assert_frozen(
        name,
        "read",
        &Value::String(rs.get("read").and_then(Value::as_str).unwrap_or("").to_string()),
        &Value::String(golden.get("read").and_then(Value::as_str).unwrap_or("").to_string()),
    );

    assert_frozen(
        name,
        "strict",
        &json_pick(&rs, "strict", "strict_error"),
        &json_pick(&golden, "strict", "strict_error"),
    );

    let rs_steps = normalize_volatile(rs.get("steps").unwrap_or(&Value::Null));
    let golden_steps = normalize_volatile(golden.get("steps").unwrap_or(&Value::Null));
    assert_frozen(name, "steps", &rs_steps, &golden_steps);

    let rs_file = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(rs.get("file").and_then(Value::as_str).unwrap_or(""))
            .expect("rs file b64"),
    )
    .expect("rs file bytes are utf8");
    let golden_file = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(golden.get("file").and_then(Value::as_str).unwrap_or(""))
            .expect("golden file b64"),
    )
    .expect("golden file bytes are utf8");
    let rs_root: Value = serde_json::from_str(&rs_file).expect("rs file parses");
    let golden_root: Value = serde_json::from_str(&golden_file).expect("golden file parses");
    // Byte identity, checked structurally then textually: the structural
    // compare localizes a divergence; the textual one on normalized bytes
    // catches ordering the structural compare forgives.
    assert_frozen(name, "file-content", &normalize_volatile(&rs_root), &normalize_volatile(&golden_root));
    assert_eq!(
        normalize_bytes(&rs_file), normalize_bytes(&golden_file),
        "{name}: the published file must be byte-identical to the golden modulo volatile stamps\n--- live ---\n{rs_file}\n--- golden ---\n{golden_file}"
    );
}

/// Exactly one of the two keys exists; return whichever does, so a golden
/// that recorded a strict-read SUCCESS compares against the live one the
/// same way a golden that recorded a FAILURE does.
fn json_pick(v: &Value, ok_key: &str, err_key: &str) -> Value {
    v.get(ok_key)
        .or_else(|| v.get(err_key))
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn characterization_reads_and_mutations_match_the_frozen_python_leg() {
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
fn characterization_legacy_rows_and_defer_backfill_match() {
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
fn characterization_unicode_and_escapes_are_byte_identical() {
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
fn characterization_session_lifecycle_matches() {
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
fn characterization_related_mirror_matches() {
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
fn corrupt_and_malformed_roots_keep_the_read_failure_taxonomy() {
    // The soft read swallows corruption to []; the strict read raises, and
    // the RAISED KIND is the taxonomy the Python strict read fixed. No
    // golden: the contract is the kind, not bytes.
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
        let rs = rust_probe(&graph, &ops);
        assert_eq!(
            rs.get("read").and_then(Value::as_str),
            Some("[]"),
            "{name}: soft read swallows"
        );
        assert_eq!(
            rs.get("strict_error").and_then(Value::as_str),
            Some(want_kind),
            "{name}: strict kind"
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
    let final_entries = graph_store::read_defaulted(&graph, false).unwrap();
    let ids: Vec<String> = final_entries
        .iter()
        .filter_map(|e| graph_store::entry_id(e).map(str::to_string))
        .collect();
    for i in 0..8 {
        let want = format!("ab-c0nc{i:04}");
        assert!(
            ids.iter().any(|id| *id == want),
            "writer {i}'s node must survive: {ids:?}"
        );
    }
}
