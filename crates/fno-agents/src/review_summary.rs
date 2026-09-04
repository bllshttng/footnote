//! `fno-agents review-summary`: the one human display line for a reviewed head.
//!
//! The merge gate reads the ledger (`.fno/events.jsonl`) and never the PR
//! body, so the body may carry a claim only THIS verb authors: it reads the
//! same ledger and prints the reviewed-at line for a branch whose latest
//! attestation is a `pass` pinned to `--head`. Any other state - a fail, a
//! stale head, a missing or unreadable events file - prints nothing and
//! exits 0, so a PR that arrives unreviewed carries no claim. A display line
//! can never clear a gate; the gate keeps its own read.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// One selected `review_attestation` row, reduced to the fields the line needs.
struct AttestationRow {
    head_sha: String,
    verdict: String,
    review_round: Option<u64>,
    findings: u64,
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    for (idx, a) in args.iter().enumerate() {
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
        if a == flag {
            return args.get(idx + 1).map(|v| v.to_string());
        }
    }
    None
}

fn parse_args(args: &[String]) -> Option<(PathBuf, String, String)> {
    // A caller that cannot name all three gets silence, not a guess: the
    // verb's only output is a claim about a specific (branch, head) pair.
    Some((
        PathBuf::from(flag_value(args, "--events")?),
        flag_value(args, "--branch")?,
        flag_value(args, "--head")?,
    ))
}

/// Prefix sha match on the shorter side, minimum 7 hex chars, the tolerance
/// `attestation_in_scope` callers use when a display surface hands a short
/// sha. Shorter than 7 demands exact equality.
fn sha_matches(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let n = a.len().min(b.len());
    // get() (not slice indexing): a non-hex ledger field must degrade to a
    // non-match, never panic the display verb.
    n >= 7 && a.get(..n) == b.get(..n)
}

fn select_rows(events_text: &str, branch: &str) -> Vec<AttestationRow> {
    let mut rows = Vec::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let row_branch = val.pointer("/data/branch").and_then(|v| v.as_str());
        if row_branch != Some(branch) {
            continue;
        }
        let head_sha = val
            .pointer("/data/head_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let verdict = val
            .pointer("/data/verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Missing counts as zero on every numeric field; a wrong-typed field
        // is the same as a missing one here (the emit-side validator is what
        // keeps these integers, and this reader only composes display text).
        let review_round = val.pointer("/data/review_round").and_then(|v| v.as_u64());
        let blocking = val
            .pointer("/data/findings_blocking")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let nonblocking = val
            .pointer("/data/findings_nonblocking")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        rows.push(AttestationRow {
            head_sha,
            verdict,
            review_round,
            findings: blocking + nonblocking,
        });
    }
    rows
}

/// The display line for a branch/head pair, or `None` when the ledger does
/// not hold a clean attestation at exactly that head. Events are read once
/// and kept in append order, so the LAST row for the branch is the latest.
pub fn summary_line(events_text: &str, branch: &str, head: &str) -> Option<String> {
    let rows = select_rows(events_text, branch);
    let latest = rows.last()?;
    if latest.verdict != "pass" || !sha_matches(&latest.head_sha, head) {
        return None;
    }
    let rounds = match rows.iter().filter_map(|r| r.review_round).max() {
        Some(max) => max,
        None => {
            // No event carries review_round: fall back to counting the
            // distinct heads the branch was reviewed at.
            let heads: BTreeSet<&str> = rows.iter().map(|r| r.head_sha.as_str()).collect();
            heads.len() as u64
        }
    };
    let findings: u64 = rows.iter().map(|r| r.findings).sum();
    Some(format!(
        "Reviewed at {head}: {rounds} rounds, {findings} findings disposed."
    ))
}

/// `fno-agents review-summary` entry: prints the line or nothing, exit 0
/// either way. A missing or unreadable events file is an EMPTY ledger, not
/// an error - the PR opens without the section and the gate still reads the
/// ledger at merge time.
pub fn run_review_summary(args: &[String]) -> i32 {
    let Some((events_path, branch, head)) = parse_args(args) else {
        return 0;
    };
    let Ok(events_text) = std::fs::read_to_string(events_path) else {
        return 0;
    };
    if let Some(line) = summary_line(&events_text, &branch, &head) {
        println!("{line}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attestation(
        branch: &str,
        head: &str,
        verdict: &str,
        round: Option<u64>,
        findings: u64,
    ) -> String {
        let round_part = match round {
            Some(r) => format!(r#", "review_round": {r}"#),
            None => String::new(),
        };
        let (blocking, nonblocking) = if findings > 0 { (findings, 0) } else { (0, 0) };
        format!(
            r#"{{"type": "review_attestation", "data": {{"reviewer": "code-review", "branch": "{branch}", "head_sha": "{head}", "verdict": "{verdict}", "findings_blocking": {blocking}, "findings_nonblocking": {nonblocking}{round_part}}}}}"#
        )
    }

    #[test]
    fn two_rounds_with_findings_print_the_disposed_line() {
        let events = format!(
            "{}\n{}\n",
            attestation("feature/x", "aaa1111", "fail", Some(1), 2),
            attestation("feature/x", "abc", "pass", Some(2), 1),
        );
        let line = summary_line(&events, "feature/x", "abc").expect("a pass at head prints");
        assert_eq!(line, "Reviewed at abc: 2 rounds, 3 findings disposed.");
    }

    #[test]
    fn no_round_field_counts_distinct_heads() {
        let events = format!(
            "{}\n{}\n",
            attestation("feature/x", "aaa1111", "fail", None, 2),
            attestation("feature/x", "bbb2222", "pass", None, 1),
        );
        let line = summary_line(&events, "feature/x", "bbb2222").expect("prints");
        assert_eq!(line, "Reviewed at bbb2222: 2 rounds, 3 findings disposed.");
    }

    #[test]
    fn a_fail_verdict_prints_nothing() {
        let events = format!(
            "{}\n{}\n",
            attestation("feature/x", "aaa1111", "pass", Some(1), 0),
            attestation("feature/x", "bbb2222", "fail", Some(2), 1),
        );
        assert_eq!(summary_line(&events, "feature/x", "bbb2222"), None);
    }

    #[test]
    fn a_pass_on_another_head_prints_nothing() {
        let events = format!(
            "{}\n",
            attestation("feature/x", "aaa1111", "pass", Some(1), 0),
        );
        assert_eq!(summary_line(&events, "feature/x", "bbb2222"), None);
    }

    #[test]
    fn another_branchs_passes_are_not_mine() {
        let events = format!(
            "{}\n{}\n",
            attestation("feature/other", "aaa1111", "pass", Some(1), 5),
            attestation("feature/x", "bbb2222", "pass", Some(1), 0),
        );
        let line = summary_line(&events, "feature/x", "bbb2222").expect("prints");
        assert_eq!(line, "Reviewed at bbb2222: 1 rounds, 0 findings disposed.");
    }

    #[test]
    fn missing_file_prints_nothing_and_exits_zero() {
        let code = run_review_summary(&[
            "review-summary".to_string(),
            "--events".to_string(),
            "/nonexistent/fno-review-summary-test/events.jsonl".to_string(),
            "--branch".to_string(),
            "feature/x".to_string(),
            "--head".to_string(),
            "abc1234".to_string(),
        ]);
        assert_eq!(code, 0);
    }

    #[test]
    fn missing_args_print_nothing_and_exit_zero() {
        assert_eq!(run_review_summary(&["review-summary".to_string()]), 0);
        assert_eq!(
            run_review_summary(&[
                "review-summary".to_string(),
                "--events".to_string(),
                "e.jsonl".to_string(),
            ]),
            0
        );
    }
}
