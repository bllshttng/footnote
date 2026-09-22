//! parity-stage: characterization
//! parity-oracle: fno.outstanding.cli.ask (the deleted Python body, goldens
//! captured from the canonical checkout before the shim swap)
//!
//! Characterization tests for the `question-intake` Rust port of
//! `fno inbox outstanding ask`. The contract is the Python leg's observable
//! outcome: stdout carries the new id and the exit code matches, byte for
//! byte. The minted id is random, so both sides normalize `q-<8 hex>` to
//! `q-XXXXXXXX` before the compare.
//!
//! Goldens live under `tests/golden/question_intake/<case>.{exit,out,err}`
//! and were captured from the Python leg by a one-off run against the
//! canonical checkout (the old body no longer exists in the tree to wire an
//! in-test oracle; each capture asserted its expected shape before the
//! freeze). Regenerating them requires that checkout.

use common::{assert_golden, golden_dir, Golden};
use fno_agents::question_intake::{run_intake, IntakeRequest};
use std::fs;
use std::path::{Path, PathBuf};

mod common;

const LAW_ROW: &str = "d-parity0001";

fn normalize(text: &str) -> String {
    // `q-` + 8 lowercase hex -> the placeholder, everywhere it appears.
    let is_qhex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find("q-") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + 2..];
        let hex: Vec<char> = tail.chars().take(8).collect();
        if hex.len() == 8 && hex.iter().all(|c| is_qhex(*c)) {
            out.push_str("q-XXXXXXXX");
            rest = &tail[8..];
        } else {
            out.push_str("q-");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "fno-agents-qintake-parity-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

fn fixture() -> (PathBuf, PathBuf) {
    let root = tmp_dir("root");
    let fno_home = tmp_dir("home");
    fs::create_dir_all(fno_home.join("agents")).unwrap();
    // The live law the refusal case matches (authority_source operator past
    // the lane cutover reads lane=law, lifecycle=live).
    fs::write(
        fno_home.join("decisions.jsonl"),
        format!(
            r#"{{"ts":"2026-09-20T00:00:00Z","decision_id":"{LAW_ROW}","subject":"parity-subject","decision":"stay strict","authority_source":"operator"}}"#
        ),
    )
    .unwrap();
    (root, fno_home)
}

fn request(question: &str, root: &Path) -> IntakeRequest {
    IntakeRequest {
        question: question.to_string(),
        ask: None,
        options: vec![],
        blocks: vec![],
        node: None,
        subject: None,
        session_id: Some("s-parity".to_string()),
        cwd: Some("/repo/fno".to_string()),
        asker: Some("worker-parity".to_string()),
        laws: vec![],
        storage_root: root.to_path_buf(),
        index_path: None,
        journal_path: None,
        display_name: None,
        render_cap: None,
    }
}

/// The contract stream is stdout (the new id line, or empty on a refusal);
/// the exit code rides `Golden.exit`. The shim's stderr prose is human
/// wording, not part of the frozen contract.
fn golden_of(answer: &fno_agents::question_intake::IntakeAnswer) -> Golden {
    let stdout = match (&answer.qid, answer.exit_code) {
        (Some(qid), 0) => format!("{qid}\n"),
        _ => String::new(),
    };
    Golden {
        exit: Some(answer.exit_code),
        streams: vec![normalize(&stdout)],
    }
}

#[test]
fn plain_ask_matches_the_python_leg() {
    let (root, fno_home) = fixture();
    let home = fno_agents::paths::AgentsHome::at(&fno_home.join("agents"));
    let answer = run_intake(&request("parity fixture question one", &root), &home);
    assert_golden("question_intake", "plain_ask", &golden_of(&answer), None);
}

#[test]
fn ask_with_options_matches_the_python_leg() {
    let (root, fno_home) = fixture();
    let home = fno_agents::paths::AgentsHome::at(&fno_home.join("agents"));
    let mut r = request("parity fixture question two", &root);
    r.options = vec!["a".to_string(), "b".to_string()];
    r.node = Some("x-aaaa".to_string());
    let answer = run_intake(&r, &home);
    assert_golden(
        "question_intake",
        "ask_with_options",
        &golden_of(&answer),
        None,
    );
}

#[test]
fn refused_by_live_law_matches_the_python_leg() {
    let (root, fno_home) = fixture();
    let home = fno_agents::paths::AgentsHome::at(&fno_home.join("agents"));
    let mut r = request("parity-subject question three", &root);
    r.subject = Some("parity-subject".to_string());
    r.laws = vec![serde_json::from_str(&format!(
        r#"{{"decision_id":"{LAW_ROW}","subject":"parity-subject","decision":"stay strict","ts":"2026-09-20T00:00:00Z"}}"#
    ))
    .unwrap()];
    let answer = run_intake(&r, &home);
    assert_eq!(answer.exit_code, 2, "the fixture law must refuse");
    assert_golden(
        "question_intake",
        "refused_by_live_law",
        &golden_of(&answer),
        None,
    );
}

#[test]
fn over_cap_question_matches_the_python_leg() {
    let (root, fno_home) = fixture();
    let home = fno_agents::paths::AgentsHome::at(&fno_home.join("agents"));
    let long = "x".repeat(2050);
    let answer = run_intake(&request(&long, &root), &home);
    assert_golden(
        "question_intake",
        "over_cap_question",
        &golden_of(&answer),
        None,
    );
}

/// The goldens this test reads (kept as a data check, not a no-op glob):
/// four frozen cases must exist or the suite has no contract.
#[test]
fn golden_corpus_is_complete() {
    let dir = golden_dir("question_intake");
    for case in [
        "plain_ask",
        "ask_with_options",
        "refused_by_live_law",
        "over_cap_question",
    ] {
        assert!(
            dir.join(format!("{case}.exit")).is_file(),
            "missing frozen golden for {case} in {}",
            dir.display()
        );
    }
}
