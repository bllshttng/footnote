//! Characterization for the reconcile-lane port (protocol steps 2-4,
//! docs/architecture/dual-implementation-inventory.md). The Rust leg is
//! `fno_agents::fleet_task::reconcile`; the Python leg it freezes against was
//! the fold inside `cli/src/fno/agents/stale_escalate.py`
//! (`reconcile_channel` + `answered_question`/`reset_answered`), whose body
//! became one `verb_call` in the same change that froze these goldens.
//!
//! The two legs file DIFFERENT row families (operator_question vs
//! fleet_task), so the frozen contract is the normalized projection, not
//! store bytes: the outcome word, the count of close rows, and the sorted
//! open keys. Close-row reasons deliberately differ (the plan names the new
//! vocabulary) and stay outside the projection.
//!
//! Goldens live under `tests/golden/fleet_task_reconcile/`. To capture them
//! (only meaningful while the Python fold still exists), run with
//! `FNO_CAPTURE_GOLDEN=1`: the helper then runs the Python leg on the
//! state-equivalent fixture, asserts Rust==Python, and freezes Python's
//! projection.

//! parity-stage: characterization
//! parity-oracle: fno.agents.stale_escalate.answered_question

use common::{assert_golden, capture_mode, Golden};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

/// Repo `cli/src` so Python can import the real `fno` package.
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

/// The same key both legs dedupe on: Python `dedupe_key` is
/// sha256(sorted-uniq identities joined with newlines), first 12 hex chars.
fn dedupe_key(identities: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut uniq: Vec<&str> = identities.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    let hex: String = Sha256::digest(uniq.join("\n").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex[..12].to_string()
}

const LANE: &str = "watchdog-stale";
const MARKER: &str = "watchdog-stale";
const SUBJECT: &str = "stale set";
const CWD: &str = "/r";

fn seed_fleet_rows(store: &Path, keys: &[&str]) {
    if keys.is_empty() {
        return;
    }
    let mut text = String::new();
    for (i, key) in keys.iter().enumerate() {
        text.push_str(&format!(
            "{}\n",
            json!({
                "ts": "2026-09-22T10:00:00Z",
                "type": "fleet_task",
                "source": "daemon",
                "data": {
                    "task_id": format!("ft-seed{i:04x}"),
                    "lane": LANE,
                    "key": key,
                    "cwd": CWD,
                    "text": format!("chore {key}"),
                    "run": format!("run({key})"),
                },
            })
        ));
    }
    use std::io::Write;
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// The Python leg's state-equivalent fixture: the same open keys as
/// operator_question rows carrying the marker needle.
fn seed_question_rows(store: &Path, keys: &[&str]) {
    if keys.is_empty() {
        return;
    }
    let mut text = String::new();
    for (i, key) in keys.iter().enumerate() {
        text.push_str(&format!(
            "{}\n",
            json!({
                "ts": "2026-09-22T10:00:00Z",
                "type": "operator_question",
                "source": "daemon",
                "data": {
                    "question_id": format!("q-seed{i:04x}"),
                    "question": format!("[{MARKER}:{key}] stale rows: qtext({key}). clear: run({key})"),
                    "cwd": CWD,
                    "ask": format!("run({key})"),
                },
            })
        ));
    }
    use std::io::Write;
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// The normalized contract both legs are read in: open keys sorted, close
/// rows counted, ids and stamps gone.
fn projection(text: &str) -> Value {
    let mut asked: Vec<(String, String)> = Vec::new();
    let mut fleet: Vec<(String, String)> = Vec::new();
    let mut closed: BTreeSet<String> = BTreeSet::new();
    let needle = format!("[{MARKER}:");
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let data = v.get("data").cloned().unwrap_or_else(|| json!({}));
        let id = data
            .get("question_id")
            .or_else(|| data.get("task_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match kind {
            "operator_question" => {
                if let Some(q) = data.get("question").and_then(Value::as_str) {
                    if let Some(i) = q.find(&needle) {
                        let key = q[i + needle.len()..].split(']').next().unwrap_or("");
                        asked.push((id, key.to_string()));
                    }
                }
            }
            "fleet_task" => {
                if let Some(k) = data.get("key").and_then(Value::as_str) {
                    fleet.push((id, k.to_string()));
                }
            }
            "operator_question_closed" | "fleet_task_closed" => {
                closed.insert(id);
            }
            _ => {}
        }
    }
    let mut keys: Vec<String> = asked
        .iter()
        .filter(|(id, _)| !closed.contains(id))
        .map(|(_, k)| k.clone())
        .collect();
    keys.extend(
        fleet
            .iter()
            .filter(|(id, _)| !closed.contains(id))
            .map(|(_, k)| k.clone()),
    );
    keys.sort();
    keys.dedup();
    json!({"closed": closed.len(), "keys": keys})
}

/// The Python fold on the state-equivalent fixture; capture mode only.
/// Prints `outcome|projection` on stdout, panics with its stderr otherwise.
fn python_leg(keys: &[&str], identities: &[&str], empty: bool, dir: &Path) -> Golden {
    let store = dir.join("py-questions.jsonl");
    std::fs::remove_file(&store).ok();
    seed_question_rows(&store, keys);
    let proj_root = dir.join("py-proj");
    std::fs::create_dir_all(proj_root.join(".fno")).unwrap();
    let driver = r#"
import json, os, sys
from pathlib import Path
store = Path(os.environ["STORE"])
root = Path(os.environ["PROJ_ROOT"])
ops = json.loads(os.environ["OPS"])
if not store.exists():
    store.write_text("")
import fno.paths
fno.paths.questions_jsonl = lambda: store
from fno.agents.stale_escalate import reconcile_channel
pairs = [] if ops["empty"] else [ops["identities"][0]]
outcome, _qid = reconcile_channel(
    pairs, root=root, session_id=None, cwd=Path(ops["cwd"]),
    marker=ops["marker"], subject=ops["subject"], identities=ops["identities"],
    question=lambda k: "[%s:%s] qtext(%s)" % (ops["marker"], k, k),
    ask=lambda k: "run(%s)" % k, asker=None,
)
needle = "[%s:" % ops["marker"]
from fno.outstanding.core import read_open_questions, read_question_events
asked = {}
for q in read_open_questions(root):
    i = q.question.find(needle)
    if i >= 0:
        asked[q.id] = q.question[i + len(needle):].split("]")[0]
closed = {rec["data"]["question_id"] for rec in read_question_events()
          if rec.get("type") == "operator_question_closed"
          and rec.get("data", {}).get("question_id")}
keys = sorted({k for qid2, k in asked.items() if qid2 not in closed})
print(json.dumps({"closed": len(closed), "keys": keys}, sort_keys=True, separators=(",", ":")))
sys.stderr.write(outcome + "\n")
"#;
    let out = Command::new(python_executable())
        .arg("-c")
        .arg(driver)
        .env("PYTHONPATH", pythonpath())
        .env("STORE", &store)
        .env("PROJ_ROOT", &proj_root)
        .env(
            "OPS",
            json!({
                "empty": empty,
                "identities": identities,
                "cwd": CWD,
                "marker": MARKER,
                "subject": SUBJECT,
            })
            .to_string(),
        )
        .output()
        .expect("run python reconcile_channel");
    assert!(
        out.status.success(),
        "python leg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let outcome = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let projection = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Golden {
        exit: None,
        streams: vec![format!("{outcome}|{projection}")],
    }
}

/// One case through both legs. `keys` seeds the open lane set; `identities`
/// and `empty` drive the reconcile.
fn run_case(label: &str, keys: &[&str], identities: &[&str], empty: bool) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("questions.jsonl");
    seed_fleet_rows(&store, keys);
    let key = identities
        .first()
        .map(|_| dedupe_key(identities))
        .unwrap_or_default();
    let (outcome, _id) = fno_agents::fleet_task::reconcile(
        &store,
        LANE,
        &key,
        CWD,
        &format!("qtext({key})"),
        &format!("run({key})"),
        empty,
    )
    .unwrap();
    // The none-case never writes the store, so the read may legitimately
    // miss: an absent store and an empty one fold the same.
    let proj = projection(&std::fs::read_to_string(&store).unwrap_or_default());
    let rust = Golden {
        exit: None,
        streams: vec![format!("{outcome}|{proj}")],
    };
    let oracle = if capture_mode() {
        Some(python_leg(keys, identities, empty, dir.path()))
    } else {
        None
    };
    assert_golden("fleet_task_reconcile", label, &rust, oracle);
}

#[test]
fn a_new_set_asks_and_files_one_task() {
    run_case("a new set asks and files one task", &[], &["i1"], false);
}

#[test]
fn the_same_set_is_a_duplicate() {
    let key = dedupe_key(&["i1"]);
    run_case("the same set is a duplicate", &[&key], &["i1"], false);
}

#[test]
fn a_changed_set_supersedes_then_asks() {
    let k1 = dedupe_key(&["i1"]);
    run_case("a changed set supersedes then asks", &[&k1], &["i2"], false);
}

#[test]
fn an_empty_set_closes_every_open_task() {
    let k1 = dedupe_key(&["i1"]);
    let k2 = dedupe_key(&["i2"]);
    run_case(
        "an empty set closes every open task",
        &[&k1, &k2],
        &["i1"],
        true,
    );
}

#[test]
fn an_empty_set_over_nothing_open_reads_none() {
    run_case(
        "an empty set over nothing open reads none",
        &[],
        &["i1"],
        true,
    );
}
