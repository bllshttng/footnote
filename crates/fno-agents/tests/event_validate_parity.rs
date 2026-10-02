//! parity-stage: differential
//! parity-oracle: fno.events.validate
//!
//! Differential parity: the Python judge (cli/src/fno/events/__init__.py,
//! `def validate`) is the live oracle; the native judge in
//! crates/fno-agents/src/event_store/validate.rs must answer every corpus
//! row with the same verdict and the same one-line diagnostic. Goldens
//! freeze the Python verdict per row (276 rows in
//! cli/tests/events/parity_corpus.jsonl); capture mode (FNO_CAPTURE_GOLDEN=1)
//! re-derives them from the live Python and asserts Rust==Python at
//! freeze time, so the frozen goldens are proven current, not inherited.
//! When the Python judge is deleted (the reader families port), this file
//! flips to `parity-stage: characterization` and the goldens stand alone.
//!
//! AC2-ERR: when Python is absent the capture path skips the row and
//! never fails the run.

use common::{assert_golden, capture_mode, Golden};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

/// Repo root: crates/fno-agents -> repo.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// (label, event JSON) per corpus row.
fn corpus_rows() -> Vec<(String, String)> {
    let path = repo_root().join("cli/tests/events/parity_corpus.jsonl");
    let text = fs::read_to_string(path).expect("parity corpus is present");
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: serde_json::Value = serde_json::from_str(line).expect("corpus row parses");
        let label = row["reason"]
            .as_str()
            .expect("corpus rows carry reasons")
            .to_string();
        rows.push((
            label,
            serde_json::to_string(&row["event"]).expect("event reserializes"),
        ));
    }
    rows
}

/// The Python judge's verdict for one corpus row: (exit, stderr). None
/// when Python or the judge cannot run (AC2-ERR: the capture path skips
/// and never fails).
fn run_python_verdict(tmp: &Path, event_json: &str) -> Option<Golden> {
    std::fs::write(tmp, event_json).ok()?;
    let output = Command::new("python3")
        .arg(driver_path())
        .arg(tmp)
        .env(
            "PYTHONPATH",
            repo_root().join("cli/src").canonicalize().ok()?,
        )
        .output()
        .ok()?;
    Some(Golden {
        exit: output.status.code(),
        streams: vec![
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ],
    })
}

/// Path to the Python-side capture driver.
fn driver_path() -> PathBuf {
    repo_root()
        .join("crates/fno-agents/tests/event_validate_driver.py")
        .to_path_buf()
}

/// The native judge's verdict for one corpus row, as a Golden.
fn run_rust_verdict(event_json: &str) -> Golden {
    match fno_agents::event_store::validate::judge_line(event_json) {
        fno_agents::event_store::validate::Verdict::Valid => Golden {
            exit: Some(0),
            streams: vec![String::new(), String::new()],
        },
        fno_agents::event_store::validate::Verdict::Invalid(msg) => Golden {
            exit: Some(1),
            streams: vec![String::new(), format!("{msg}\n")],
        },
        fno_agents::event_store::validate::Verdict::Substrate(msg) => Golden {
            exit: Some(2),
            streams: vec![String::new(), format!("{msg}\n")],
        },
    }
}

/// Every corpus row, one golden key each: in capture mode the Python
/// verdict freezes (and the native judge must match it at freeze time);
/// otherwise the frozen golden is the contract and Python never runs.
#[test]
fn parity_over_corpus() {
    let tmp = tempfile::TempDir::new().unwrap();
    let payload = tmp.path().join("event.json");
    for (label, event_json) in corpus_rows() {
        let rust = run_rust_verdict(&event_json);
        let oracle = if capture_mode() {
            run_python_verdict(&payload, &event_json)
        } else {
            None
        };
        if capture_mode() && oracle.is_none() {
            // AC2-ERR: Python absent -> the capture path skips the row.
            continue;
        }
        assert_golden("event_validate", &label, &rust, oracle);
    }
}
