//! Characterization tests for the `verify-evidence` Rust port, frozen against
//! the bash oracle `scripts/lib/verify-event-evidence.sh` (packaging EPIC
//! ab-8bdb4642).
//!
//! These were originally DIFFERENTIAL parity tests that ran BOTH the bash
//! oracle and the Rust verb over identical fixtures and asserted byte-equality.
//! The bash oracle has since been deleted (the Rust port is the sole
//! implementation), so each case now asserts the Rust output against a GOLDEN
//! `(exit, stdout, stderr)` captured from the proven-correct bash BEFORE
//! deletion.
//!
//! Goldens live under `tests/golden/verify_evidence/<case>.{exit,out,err}`,
//! keyed by a slug of each case's label. To regenerate them (only meaningful
//! while the bash oracle still exists), run with `FNO_CAPTURE_GOLDEN=1`: the
//! helper then runs bash, writes the golden files, AND asserts Rust==bash
//! before freezing.
//!
//! Coverage (AC1-EDGE / AC1-ERR):
//!   verify_child_promise: valid (rc0), nonce-mismatch (rc1), missing (rc1),
//!     unreadable (rc2).
//!   resolve_has_nonclaud_agent: non-claude agent (rc0), all-claude (rc1),
//!     settings-absent (rc2), dangling-provider-ref (warn + skip), malformed
//!     YAML (rc1 + warn).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Slug a case label into a filename-safe golden key (lowercase, every run of
/// non-alphanumeric chars collapses to a single `_`, trimmed).
fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_us = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Directory holding the frozen verify_evidence goldens.
fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/verify_evidence")
}

/// Whether to (re)capture goldens from the live bash oracle this run.
fn capture_mode() -> bool {
    std::env::var("FNO_CAPTURE_GOLDEN").is_ok()
}

/// Absolute path to the bash oracle (only used in capture mode).
fn bash_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts/lib/verify-event-evidence.sh")
}

/// Run a bash function from the oracle with the given args.
/// Returns (exit_code, stdout, stderr). Only invoked in capture mode.
fn run_bash(func: &str, args: &[&str]) -> (i32, String, String) {
    let quoted: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
    let cmd = format!(
        "source '{}'; {} {}",
        bash_script().display(),
        func,
        quoted.join(" ")
    );
    let out = Command::new("bash")
        .args(["-c", &cmd])
        .output()
        .expect("run bash oracle");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run the Rust port in-process. `sub` is the sub-verb; `args` follows it.
/// Returns (exit_code, stdout, stderr).
fn run_rust(sub: &str, args: &[&str]) -> (i32, String, String) {
    let mut argv: Vec<String> = vec![sub.to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    fno_agents::verify_evidence::run_verify_evidence_capture(&argv)
}

/// Replace volatile fixture paths in an output stream with a stable token.
///
/// Two diagnostics embed the absolute fixture path verbatim (the
/// events-file-unreadable and the malformed-settings WARN), and `tempfile`
/// mints a fresh random tempdir on every run. Freezing the literal path would
/// make those goldens un-rematchable, so both the captured golden and the live
/// Rust output have each path-like arg replaced with `<FIXTURE>` before
/// write/compare. Longest args first so a shorter arg can't partial-match a
/// longer one. Non-path args (session ids, nonces) never contain `/`, so they
/// are left untouched and still asserted byte-for-byte.
fn normalize_paths(s: &str, args: &[&str]) -> String {
    let mut paths: Vec<&&str> = args.iter().filter(|a| a.contains('/')).collect();
    paths.sort_by_key(|a| std::cmp::Reverse(a.len()));
    let mut out = s.to_string();
    for p in paths {
        out = out.replace(p, "<FIXTURE>");
    }
    out
}

/// Core golden assertion: assert the Rust `(exit, stdout, stderr)` for a sub-verb
/// equals the frozen golden for this case.
///
/// In capture mode (`FNO_CAPTURE_GOLDEN=1`), runs the bash `func` on the SAME
/// args, writes the golden files, and additionally asserts Rust==bash so a
/// broken capture is caught at freeze time. In normal mode (the deleted-bash
/// world), reads the golden and asserts Rust matches it — bash is never run.
///
/// Volatile fixture paths are normalized to `<FIXTURE>` on both sides (see
/// `normalize_paths`) so the frozen goldens survive the per-run tempdir churn.
fn assert_golden(sub: &str, bash_func: &str, args: &[&str], label: &str) {
    let key = slug(label);
    let dir = golden_dir();
    let exit_path = dir.join(format!("{key}.exit"));
    let out_path = dir.join(format!("{key}.out"));
    let err_path = dir.join(format!("{key}.err"));

    let (rc, ro, re) = run_rust(sub, args);
    let ro = normalize_paths(&ro, args);
    let re = normalize_paths(&re, args);

    if capture_mode() {
        let (bc, bo, be) = run_bash(bash_func, args);
        let bo = normalize_paths(&bo, args);
        let be = normalize_paths(&be, args);
        assert_eq!(bc, rc, "[{label}] capture: exit bash={bc} rust={rc}");
        assert_eq!(
            bo, ro,
            "[{label}] capture: stdout\nbash={bo:?}\nrust={ro:?}"
        );
        assert_eq!(
            be, re,
            "[{label}] capture: stderr\nbash={be:?}\nrust={re:?}"
        );
        fs::create_dir_all(&dir).unwrap();
        fs::write(&exit_path, format!("{bc}\n")).unwrap();
        fs::write(&out_path, &bo).unwrap();
        fs::write(&err_path, &be).unwrap();
        return;
    }

    let golden_exit: i32 = fs::read_to_string(&exit_path)
        .unwrap_or_else(|e| panic!("[{label}] missing golden {exit_path:?}: {e}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("[{label}] bad golden exit in {exit_path:?}: {e}"));
    let golden_out = fs::read_to_string(&out_path)
        .unwrap_or_else(|e| panic!("[{label}] missing golden {out_path:?}: {e}"));
    let golden_err = fs::read_to_string(&err_path)
        .unwrap_or_else(|e| panic!("[{label}] missing golden {err_path:?}: {e}"));

    assert_eq!(
        golden_exit, rc,
        "[{label}] exit differs from golden: golden={golden_exit} rust={rc}"
    );
    assert_eq!(
        golden_out, ro,
        "[{label}] stdout differs from golden:\ngolden={golden_out:?}\nrust={ro:?}"
    );
    assert_eq!(
        golden_err, re,
        "[{label}] stderr differs from golden:\ngolden={golden_err:?}\nrust={re:?}"
    );
}

/// Assert the child-promise sub-verb matches the frozen `verify_child_promise`
/// golden.
fn assert_child_parity(events: &str, sid: &str, nonce: &str, label: &str) {
    assert_golden(
        "child-promise",
        "verify_child_promise",
        &[sid, nonce, events],
        label,
    );
}

/// Assert the has-nonclaude sub-verb matches the frozen
/// `resolve_has_nonclaud_agent` golden.
fn assert_nonclaude_parity(artifact: &str, settings: &str, label: &str) {
    assert_golden(
        "has-nonclaude",
        "resolve_has_nonclaud_agent",
        &[artifact, settings],
        label,
    );
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, content).unwrap();
    p
}

// ── verify_child_promise ──────────────────────────────────────────────────────

#[test]
fn child_promise_valid() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let events = write(
        d,
        "ev.jsonl",
        concat!(
            r#"{"ts":"x","type":"child_promise","source":"hook","data":{"session_id":"S1","nonce":"N1"}}"#,
            "\n",
        ),
    );
    assert_child_parity(events.to_str().unwrap(), "S1", "N1", "child valid -> rc0");
}

#[test]
fn child_promise_nonce_mismatch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let events = write(
        d,
        "ev.jsonl",
        concat!(
            r#"{"ts":"x","type":"child_promise","source":"hook","data":{"session_id":"S1","nonce":"N1"}}"#,
            "\n",
        ),
    );
    assert_child_parity(
        events.to_str().unwrap(),
        "S1",
        "WRONG",
        "child nonce mismatch -> rc1",
    );
}

#[test]
fn child_promise_missing_session() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let events = write(
        d,
        "ev.jsonl",
        concat!(
            r#"{"ts":"x","type":"child_promise","source":"hook","data":{"session_id":"S1","nonce":"N1"}}"#,
            "\n",
        ),
    );
    assert_child_parity(
        events.to_str().unwrap(),
        "OTHER",
        "N1",
        "child missing session -> rc1",
    );
}

#[test]
fn child_promise_unreadable_rc2() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let missing = d.join("nope.jsonl");
    assert_child_parity(
        missing.to_str().unwrap(),
        "S1",
        "N1",
        "child unreadable -> rc2",
    );
}

#[test]
fn child_promise_legacy_envelope() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    // Legacy {timestamp,...} shape still carries data.session_id + data.nonce.
    let events = write(
        d,
        "ev.jsonl",
        concat!(
            r#"{"timestamp":"x","type":"child_promise","data":{"session_id":"S2","nonce":"N9"}}"#,
            "\n",
        ),
    );
    assert_child_parity(
        events.to_str().unwrap(),
        "S2",
        "N9",
        "child legacy envelope -> rc0",
    );
}

// ── resolve_has_nonclaud_agent ────────────────────────────────────────────────

#[test]
fn nonclaude_codex_agent_rc0() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let artifact = write(d, "art.md", "agents_dispatched: [reviewer]\n");
    let settings = write(
        d,
        "config.toml",
        "[agents.reviewer]\nprovider = \"codex-prov\"\n\n[providers]\nactive = \"claude-main\"\n\n[[providers.records]]\nid = \"codex-prov\"\ncli = \"codex\"\n",
    );
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        settings.to_str().unwrap(),
        "codex agent -> rc0",
    );
}

#[test]
fn nonclaude_all_claude_rc1() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let artifact = write(d, "art.md", "agents_dispatched: [reviewer]\n");
    // Agent resolves (via global active) to a claude provider -> rc1.
    let settings = write(
        d,
        "config.toml",
        "[providers]\nactive = \"claude-main\"\n\n[[providers.records]]\nid = \"claude-main\"\ncli = \"claude\"\n",
    );
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        settings.to_str().unwrap(),
        "all-claude -> rc1",
    );
}

#[test]
fn nonclaude_settings_absent_rc2() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let artifact = write(d, "art.md", "agents_dispatched: [reviewer]\n");
    let missing = d.join("nope.yaml");
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        missing.to_str().unwrap(),
        "settings absent -> rc2",
    );
}

#[test]
fn nonclaude_dangling_provider_ref_warn_skip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let artifact = write(d, "art.md", "agents_dispatched: [reviewer]\n");
    // reviewer pins provider 'ghost' that is NOT in records -> WARN + skip ->
    // no non-claude found -> rc1.
    let settings = write(
        d,
        "config.toml",
        "[agents.reviewer]\nprovider = \"ghost\"\n\n[providers]\nactive = \"claude-main\"\n\n[[providers.records]]\nid = \"claude-main\"\ncli = \"claude\"\n",
    );
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        settings.to_str().unwrap(),
        "dangling provider ref -> warn + rc1",
    );
}

#[test]
fn nonclaude_no_agents_dispatched_rc1() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    // No agents_dispatched line -> rc1 (the bash `[[ -z ]] -> return 1`).
    let artifact = write(d, "art.md", "title: nothing\n");
    let settings = write(
        d,
        "config.toml",
        "[providers]\nactive = \"claude-main\"\n\n[[providers.records]]\nid = \"claude-main\"\ncli = \"claude\"\n",
    );
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        settings.to_str().unwrap(),
        "no agents_dispatched -> rc1",
    );
}

#[test]
fn nonclaude_malformed_yaml_warn_rc1() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    let artifact = write(d, "art.md", "agents_dispatched: [reviewer]\n");
    // Malformed TOML (an unclosed table header) -> parse fails -> WARN + rc1.
    let settings = write(d, "config.toml", "[bad\nthis = is\n");
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        settings.to_str().unwrap(),
        "malformed yaml -> warn + rc1",
    );
}

#[test]
fn nonclaude_mixed_agents_one_nonclaude_rc0() {
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path();
    // Two agents: one claude, one codex -> at least one non-claude -> rc0.
    let artifact = write(d, "art.md", "agents_dispatched: [alpha, beta]\n");
    let settings = write(
        d,
        "config.toml",
        "[agents.alpha]\nprovider = \"claude-main\"\n\n[agents.beta]\nprovider = \"codex-prov\"\n\n[providers]\nactive = \"claude-main\"\n\n[[providers.records]]\nid = \"claude-main\"\ncli = \"claude\"\n\n[[providers.records]]\nid = \"codex-prov\"\ncli = \"codex\"\n",
    );
    assert_nonclaude_parity(
        artifact.to_str().unwrap(),
        settings.to_str().unwrap(),
        "mixed agents, one non-claude -> rc0",
    );
}
