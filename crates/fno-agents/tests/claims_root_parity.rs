//! parity-stage: characterization
//! parity-oracle: fno.claims.io.claims_root_for
//!
//! Characterization harness for the one claims-root resolver. The corpus
//! (tests/golden/claims_root/corpus.out: one `key<TAB>global|none` row per
//! key) was captured from the Python leg (`fno.claims.io.claims_root_for`)
//! before deletion: every global-id prefix plus a colon-less key, a bare
//! prefix, a repo-local key, an unknown prefix, and the empty key. The
//! routing decision is the frozen contract for the sole implementation.
//!
//! Each row is driven through the real `fno-agents claim root <key>` op (the
//! read Python callers use for the path) with `FNO_CLAIMS_ROOT` set per
//! subprocess, so no process-global env races the other tests in this
//! binary: a `global` row must answer that root with `<root>/.fno/claims`
//! as the dir, and a `none` row must answer `root: null`.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

fn golden_corpus() -> Vec<(String, String)> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/claims_root/corpus.out");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden corpus {path:?}: {e}"));
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, routing) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("malformed corpus row {line:?}"));
        rows.push((key.to_string(), routing.to_string()));
    }
    assert!(
        rows.iter().any(|(k, _)| k == "node:x-golden"),
        "broken instrument: the corpus parsed without the known global key"
    );
    rows
}

fn run_root_op(key: &str, claims_root: &std::path::Path) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args(["claim", "root", key, "--json"])
        .env("FNO_CLAIMS_ROOT", claims_root)
        .output()
        .expect("run fno-agents claim root");
    let payload: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("claim root {key:?}: invalid JSON: {e}"));
    (out.status.code().unwrap_or(-1), payload)
}

#[test]
fn the_root_op_routes_every_golden_key_like_the_captured_leg() {
    let corpus = golden_corpus();
    let root = tempfile::tempdir().unwrap();
    for (key, routing) in &corpus {
        let (code, payload) = run_root_op(key, root.path());
        assert_eq!(code, 0, "claim root {key:?} exited {code}");
        assert_eq!(
            payload.get("key").and_then(Value::as_str),
            Some(key.as_str()),
            "claim root {key:?}: echoed key mismatch"
        );
        match routing.as_str() {
            "global" => {
                assert_eq!(
                    payload.get("root").and_then(Value::as_str),
                    Some(root.path().to_str().unwrap()),
                    "claim root {key:?}: expected the global root"
                );
                assert_eq!(
                    payload.get("dir").and_then(Value::as_str),
                    Some(root.path().join(".fno/claims").to_str().unwrap()),
                    "claim root {key:?}: expected dir under the global root"
                );
            }
            "none" => {
                assert!(
                    payload.get("root").map(Value::is_null).unwrap_or(false),
                    "claim root {key:?}: expected root null, got {payload}"
                );
                assert!(
                    payload.get("dir").map(Value::is_string).unwrap_or(false),
                    "claim root {key:?}: expected a resolved fallback dir"
                );
            }
            other => panic!("unknown routing verdict {other:?} in the corpus"),
        }
    }
}
