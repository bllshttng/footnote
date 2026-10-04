//! The delivered-terminal vocabulary, characterized against the Python leg it
//! replaces.
//!
//! parity-stage: characterization
//! parity-oracle: fno.terminals.DELIVERED_TERMINALS
//!
//! The Python leg (`DELIVERED_TERMINALS` in cli/src/fno/terminals.py) was the
//! frozenset the ledger promotion gate, the scoreboard fold, and the classify
//! wire all read. The port made `TerminationReason::is_delivered`
//! (loopcheck.rs) the one owner and the `fno-agents terminals` verb the read
//! door; the golden below is the sorted set the Python leg answered at
//! capture time. In capture mode (`FNO_CAPTURE_GOLDEN=1`, while the leg still
//! existed) the helper ran `python3` on the same question, asserted
//! Rust==Python, and froze the Python bytes; in normal mode the golden IS the
//! contract and Python never runs. The characterizing property: the door
//! answers exactly what the Python set named, byte for byte.

use common::{assert_golden as assert_golden_common, capture_mode, Golden};
use serde_json::Value;
use std::process::Command;

mod common;

/// The sorted delivered-terminal list the Rust door answers, one name per line.
fn run_rust() -> (i32, String) {
    let bin = env!("CARGO_BIN_EXE_fno-agents");
    let out = Command::new(bin)
        .arg("terminals")
        .output()
        .expect("run fno-agents terminals");
    if out.status.code() != Some(0) {
        return (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
    }
    let payload: Value = serde_json::from_slice(&out.stdout).expect("parse door JSON");
    let mut names: Vec<String> = payload
        .get("delivered")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    (0, names.join("\n") + "\n")
}

/// The Python leg's answer to the same question. Only invoked in capture mode.
fn run_python() -> (i32, String) {
    // The leg is stdlib-only, so it loads BY FILE PATH: importing the package
    // would drag fno/__init__ and every dependency, and the oracle needs a
    // bare interpreter to answer on any box.
    let leg = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("cli")
        .join("src")
        .join("fno")
        .join("terminals.py");
    let script = format!(
        "import importlib.util; \
         spec = importlib.util.spec_from_file_location('terminals_leg', {:?}); \
         m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); \
         print('\\n'.join(sorted(m.DELIVERED_TERMINALS)))",
        leg
    );
    let out = Command::new("python3")
        .args(["-c", &script])
        .output()
        .expect("run python oracle");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn delivered_vocabulary_matches_the_python_leg() {
    let (code, stdout) = run_rust();
    let rust = Golden {
        exit: Some(code),
        streams: vec![stdout],
    };
    let oracle = capture_mode().then(|| {
        let (py_code, py_out) = run_python();
        Golden {
            exit: Some(py_code),
            streams: vec![py_out],
        }
    });
    assert_golden_common("terminal_vocab", "delivered list", &rust, oracle);
}
