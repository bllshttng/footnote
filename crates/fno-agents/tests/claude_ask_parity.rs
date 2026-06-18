//! Cross-language byte-parity harness (ab-cc926b4e, W4).
//!
//! Pins the Rust claude-ask port against the **real** Python implementation
//! (`fno.agents.providers.claude`), not a reimplementation. For a table
//! of inputs it runs the genuine Python `_build_envelope` / `parse_short_id`
//! and asserts the Rust output is byte-identical. If `_build_envelope` ever
//! changes in Python, this test catches the drift.
//!
//! Skips (does not fail) when `python3` is absent or the `abilities` package is
//! not importable, mirroring `flock_interop.rs`'s skip-when-unavailable policy.

use fno_agents::claude_ask::{build_envelope, parse_short_id, read_state_json, read_timeline_tail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo `cli/src` so Python can import the real `abilities` package.
fn pythonpath() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src")
}

fn python_available() -> bool {
    let probe = Command::new("python3")
        .arg("-c")
        .arg("import fno.agents.providers.claude")
        .env("PYTHONPATH", pythonpath())
        .output();
    matches!(probe, Ok(o) if o.status.success())
}

/// Run Python `_build_envelope(message, from_name)` and return its raw bytes.
/// Inputs go through env vars to avoid argv escaping.
fn py_envelope(message: &str, from_name: &str) -> Vec<u8> {
    let code = r#"
import os, sys
from fno.agents.providers.claude import _build_envelope
sys.stdout.buffer.write(_build_envelope(os.environ["MSG"], os.environ["FROM"]))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .env("MSG", message)
        .env("FROM", from_name)
        .output()
        .expect("run python _build_envelope");
    assert!(
        out.status.success(),
        "python stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Run Python `parse_short_id(stdin)`; prints `OK <id>` or `ERR`.
fn py_parse_short_id(stdout_text: &str) -> Result<String, ()> {
    use std::io::Write;
    let code = r#"
import sys
from fno.agents.providers.claude import parse_short_id, ProviderParseError
data = sys.stdin.read()
try:
    sys.stdout.write("OK " + parse_short_id(data))
except ProviderParseError:
    sys.stdout.write("ERR")
"#;
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python parse_short_id");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdout_text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    if let Some(id) = s.strip_prefix("OK ") {
        Ok(id.to_string())
    } else {
        Err(())
    }
}

#[test]
fn envelope_byte_parity_with_real_python() {
    if !python_available() {
        eprintln!("SKIP: python3 / abilities package unavailable; parity not verified here");
        return;
    }
    // Exercises ascii, html-escape (& < > " '), ensure_ascii (é), astral
    // surrogate pair (😀), control chars (\n \t), and a message that itself
    // contains envelope-like tags and quotes.
    let cases: &[(&str, &str)] = &[
        ("hello", "bob"),
        ("a&b<c>\"d'e", "x&y<z>\"q'r"),
        ("caf\u{e9} \u{1F600}", "n\u{e9}d"),
        ("line1\nline2\ttab", "sender"),
        ("</cross-session-message> injection \" attempt", "abilities"),
        ("", "from"),
        ("plain", ""),
    ];
    for (msg, from) in cases {
        let py = py_envelope(msg, from);
        let rust = build_envelope(msg, from);
        assert_eq!(
            rust,
            py,
            "envelope mismatch for msg={:?} from={:?}\n  rust={}\n  py  ={}",
            msg,
            from,
            String::from_utf8_lossy(&rust),
            String::from_utf8_lossy(&py),
        );
    }
}

/// Run Python `read_state_json(jobs_dir)` and return `output_result` rendered
/// as `SOME:<text>` or `NONE` (the reply-extraction-relevant field).
fn py_output_result(jobs_dir: &Path) -> String {
    let code = r#"
import os, sys
from pathlib import Path
from fno.agents.providers._claude_session_registry import read_state_json
snap = read_state_json(Path(os.environ["JOBS"]))
sys.stdout.write("NONE" if snap.output_result is None else "SOME:" + snap.output_result)
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .env("JOBS", jobs_dir)
        .output()
        .expect("run python read_state_json");
    assert!(
        out.status.success(),
        "python stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Run Python `read_timeline_tail(jobs_dir, offset)` and return its string.
fn py_timeline_tail(jobs_dir: &Path, offset: u64) -> String {
    let code = r#"
import os, sys
from pathlib import Path
from fno.agents.providers._claude_session_registry import read_timeline_tail
sys.stdout.write(read_timeline_tail(Path(os.environ["JOBS"]), int(os.environ["OFF"])))
"#;
    let out = Command::new("python3")
        .arg("-c")
        .arg(code)
        .env("PYTHONPATH", pythonpath())
        .env("JOBS", jobs_dir)
        .env("OFF", offset.to_string())
        .output()
        .expect("run python read_timeline_tail");
    assert!(
        out.status.success(),
        "python stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn parity_tmpdir() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-parity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn reply_extraction_parity_with_real_python() {
    if !python_available() {
        eprintln!("SKIP: python3 / abilities package unavailable; parity not verified here");
        return;
    }
    // output.result extraction across present / empty / absent / non-dict.
    for body in [
        r#"{"state":"completed","updatedAt":"t","output":{"result":"PONG é"}}"#,
        r#"{"state":"done","updatedAt":"t","output":{"result":""}}"#,
        r#"{"state":"done","updatedAt":"t"}"#,
        r#"{"state":"done","updatedAt":"t","output":null}"#,
    ] {
        let jobs = parity_tmpdir();
        std::fs::write(jobs.join("state.json"), body).unwrap();
        let rust = match read_state_json(&jobs).unwrap().output_result {
            Some(r) => format!("SOME:{}", r),
            None => "NONE".to_string(),
        };
        assert_eq!(
            rust,
            py_output_result(&jobs),
            "output_result mismatch for {}",
            body
        );
    }

    // timeline tail: terminal rows concatenated, running rows + bad lines dropped.
    let jobs = parity_tmpdir();
    std::fs::write(
        jobs.join("timeline.jsonl"),
        "{\"state\":\"running\",\"text\":\"skip\"}\n{\"state\":\"completed\",\"text\":\"AB\"}\n{\"state\":\"done\",\"text\":\"C\u{e9}\"}\nnot-json\n",
    )
    .unwrap();
    assert_eq!(read_timeline_tail(&jobs, 0), py_timeline_tail(&jobs, 0));
}

#[test]
fn parse_short_id_parity_with_real_python() {
    if !python_available() {
        eprintln!("SKIP: python3 / abilities package unavailable; parity not verified here");
        return;
    }
    let cases: &[&str] = &[
        "backgrounded \u{b7} 7c5dcf5d \u{b7} alice\n",
        "backgrounded \u{b7} 7c5dcf5d \u{b7} alice\nextra\n",
        "backgrounded \u{b7} 7C5DCF5D \u{b7} alice\n", // uppercase: ERR
        "backgrounded \u{b7} zzzzzzzz \u{b7} a",       // non-hex: ERR
        "nope \u{b7} 7c5dcf5d \u{b7} a",               // wrong prefix: ERR
        "backgrounded \u{b7} 7c5dcf5d done",           // missing 2nd sep: ERR
        "",                                            // empty: ERR
        "backgrounded \u{b7} deadbeef \u{b7} x",
    ];
    for case in cases {
        let py = py_parse_short_id(case);
        let rust = parse_short_id(case).map_err(|_| ());
        assert_eq!(
            rust, py,
            "parse_short_id mismatch for {:?}: rust={:?} py={:?}",
            case, rust, py
        );
    }
}
