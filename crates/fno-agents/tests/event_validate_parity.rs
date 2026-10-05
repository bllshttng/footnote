//! parity-stage: characterization
//! parity-oracle: fno.events.validate
//!
//! Characterization tests for the native envelope judge, frozen against
//! goldens captured from the Python judge (`def validate` in
//! cli/src/fno/events/__init__.py) BEFORE that leg was deleted. The
//! goldens under tests/golden/event_validate/ freeze one
//! (exit, stdout, stderr) triple per corpus row (275 of the 276 rows in
//! cli/tests/events/parity_corpus.jsonl; the overflow-literals row is
//! answered by the door's substrate refusal and stays in the Python
//! suite). The native judge in crates/fno-agents/src/event_store/
//! validate.rs must answer every frozen case identically.
//!
//! Capture mode (FNO_CAPTURE_GOLDEN=1, only meaningful while a live
//! oracle leg exists) re-derives goldens from the oracle and asserts
//! Rust==oracle at freeze time. AC2-ERR: when Python is absent the
//! capture path skips the row and never fails the run.

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
        // Rows with literals serde_json cannot represent (non-finite
        // overflow numbers) never reach the judge: the door answers them at
        // the substrate layer and the write still never lands. Their
        // Python-side diagnostic contract stays in the Python suite until
        // that leg retires.
        let row: serde_json::Value = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue,
        };
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
    // FNO_PYTHON pins the interpreter (the project venv that has the fno
    // package deps); bare python3 is the fallback, and a driver that
    // cannot import answers exit 3, which reads as absence below.
    let interpreter = std::env::var("FNO_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let output = Command::new(interpreter)
        .arg(driver_path())
        .arg(tmp)
        .env(
            "PYTHONPATH",
            repo_root().join("cli/src").canonicalize().ok()?,
        )
        .output()
        .ok()?;
    // Only 0/1/2 are verdicts; anything else (3: driver cannot run, or a
    // crash) reads as absence and skips the row (AC2-ERR).
    if !matches!(output.status.code(), Some(0) | Some(1) | Some(2)) {
        return None;
    }
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
