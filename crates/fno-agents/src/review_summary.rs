//! `fno-agents review-summary`: the one human display line for a reviewed head.
//!
//! The merge gate reads the attestation journals and never the PR
//! body, so the body may carry a claim only THIS verb authors: it reads the
//! same journal and prints the reviewed-at line for a branch whose latest
//! attestation is a `pass` pinned to `--head`. Any other state - a fail, a
//! stale head, a missing or unreadable events file - prints nothing and
//! exits 0, so a PR that arrives unreviewed carries no claim. A display line
//! can never clear a gate; the gate keeps its own read.

use clap::Parser as _;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One selected `review_attestation` row, reduced to the fields the line needs.
#[derive(Clone)]
struct AttestationRow {
    head_sha: String,
    verdict: String,
    review_round: Option<u64>,
    findings: u64,
}

/// The journal `--events` defaults to when a caller omits it: the global
/// state root's `events.jsonl`, the mirror every attestation emission writes.
/// An external worktree cannot name its journal as `.fno/events.jsonl` -
/// there that path is the worktree's own legacy journal, not the mirror
///.
fn default_events_path() -> PathBuf {
    crate::scratch::fno_state_root().join("events.jsonl")
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

/// One pass over the journal, rows grouped under the keys `keys_of` derives
/// from each row's branch. `evidence` answers per item for a whole corpus,
/// and a per-item `select_rows` re-parse of the full text is O(items x
/// journal) - minutes on a measured 62MB journal - so the classification
/// reads one index instead.
fn index_rows_keyed<F>(events_text: &str, keys_of: F) -> BTreeMap<String, Vec<AttestationRow>>
where
    F: Fn(&str) -> Vec<String>,
{
    let mut map: BTreeMap<String, Vec<AttestationRow>> = BTreeMap::new();
    for line in events_text.lines() {
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("review_attestation") {
            continue;
        }
        let row_branch = val.pointer("/data/branch").and_then(|v| v.as_str());
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
        if let Some(branch) = row_branch {
            let row = AttestationRow {
                head_sha,
                verdict,
                review_round,
                findings: blocking + nonblocking,
            };
            for key in keys_of(branch) {
                map.entry(key).or_default().push(row.clone());
            }
        }
    }
    map
}

/// The display line reads by branch, so its index keeps the raw branch key.
fn index_rows(events_text: &str) -> BTreeMap<String, Vec<AttestationRow>> {
    index_rows_keyed(events_text, |b| vec![b.to_string()])
}

/// The evidence read classifies per NODE: a row on any branch shape (legacy
/// `feature/<id>` or `<kind>/<id>-<mini>`) proves review about its node.
fn node_keyed_rows(events_text: &str) -> BTreeMap<String, Vec<AttestationRow>> {
    index_rows_keyed(events_text, |b| crate::node_branch::node_ids(b))
}

fn select_rows(events_text: &str, branch: &str) -> Vec<AttestationRow> {
    index_rows(events_text)
        .get(branch)
        .cloned()
        .unwrap_or_default()
}

/// The round a scoped fix-verification declares: the shell emitter's stamp
/// rule (emit-attestation.sh) as one pure read. When the invocation flags
/// carry `--verify-fixes`, the pass names the round it verified instead of
/// counting as a fresh one, floored at 1 exactly like `review-hold round`.
/// Any other flag set stamps nothing: a full pass IS a round.
pub fn declared_round(
    flags_json: &str,
    events_text: &str,
    branch: &str,
    head: &str,
) -> Option<u64> {
    let flags: Vec<String> = serde_json::from_str(flags_json).ok()?;
    if !flags.iter().any(|f| f == "--verify-fixes") {
        return None;
    }
    let rounds = crate::loopcheck::rounds_since_last_pass(events_text, branch, head, None);
    Some(rounds.max(1) as u64)
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

/// The evidence read: classify each observer item by what the journal proves
/// about its review. A clean review and a missing-evidence state never share
/// a label again: `clean` is a journal row with zero findings (the review ran
/// and found nothing); every `no_*` state is missing evidence, never a
/// verified clean. Items come back in input order.
pub fn evidence(events_text: &str, items: &[Value]) -> Value {
    let index = node_keyed_rows(events_text);
    let mut out = Vec::with_capacity(items.len());
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for item in items {
        let node_val = item.get("node").cloned().unwrap_or(Value::Null);
        let pr_val = item.get("pr_number").cloned().unwrap_or(Value::Null);
        let node = node_val.as_str();
        let pr = pr_val.as_u64();
        let (state, precision): (&str, Value) = match (node, pr) {
            (None, _) => ("no_node", Value::Null),
            (Some(_), None) => ("no_pr", Value::Null),
            (Some(node), Some(_)) => {
                let rows = index.get(node).cloned().unwrap_or_default();
                if rows.is_empty() {
                    ("no_attestation", Value::Null)
                } else if rows.iter().all(|r| r.findings == 0) {
                    ("clean", Value::Null)
                } else {
                    let latest_is_pass = rows.last().map(|r| r.verdict == "pass").unwrap_or(false);
                    (
                        "scored",
                        if latest_is_pass {
                            serde_json::json!("pass")
                        } else {
                            serde_json::json!("fail")
                        },
                    )
                }
            }
        };
        *counts.entry(state).or_insert(0) += 1;
        out.push(serde_json::json!({
            "node": node_val,
            "pr_number": pr_val,
            "state": state,
            "finding_precision": precision,
        }));
    }
    let n = |s: &str| counts.get(s).copied().unwrap_or(0);
    let missing = n("no_node") + n("no_pr") + n("no_attestation");
    serde_json::json!({
        "items": out,
        "counts": counts,
        "evidence_line": format!(
            "evidence: scored={} clean={} missing={} (no_node={} no_pr={} no_attestation={})",
            n("scored"), n("clean"), missing, n("no_node"), n("no_pr"), n("no_attestation")
        ),
    })
}

/// The evidence mode's one round-trip: stdin text in (None = stdin read
/// failed), one JSON object out, `(output, exit_code)`. Unlike the display
/// line, a failure here is NOT deliberate silence - a silent evidence read is
/// the zero-coverage readout this verb exists to fix - so a non-array stdin or
/// an unreadable ledger prints `{"error": ...}` and exits 2.
fn evidence_response(events_path: &Path, stdin_text: Option<&str>) -> (String, i32) {
    let Some(text) = stdin_text else {
        return (error_json("stdin unreadable"), 2);
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return (error_json("stdin is not a JSON array"), 2);
    };
    // SQL authority: the store's committed rows are the ledger, the same read
    // the display line makes. Unreadable = error, never a silent empty ledger.
    let events_text = match crate::loopcheck::event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(e) => return (error_json(&format!("events ledger unreadable: {e}")), 2),
    };
    (evidence(&events_text, &items).to_string(), 0)
}

fn error_json(reason: &str) -> String {
    serde_json::json!({ "error": reason }).to_string()
}

/// The declared-round mode's one round-trip: `(output, exit_code)` in the
/// `evidence_response` shape, so the receipt is testable without a process
/// boundary. A declared verify prints one JSON object; any other flag set,
/// or an unreadable ledger, prints nothing and exits 0 (fail-open: the pass
/// counts as a fresh round, exactly as an undeclared pass always has).
fn declared_round_response(
    events_path: &Path,
    flags: &str,
    branch: &str,
    head: &str,
) -> (String, i32) {
    let events_text = match crate::loopcheck::event_lines(events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return (String::new(), 0),
    };
    match declared_round(flags, &events_text, branch, head) {
        Some(n) => (serde_json::json!({ "declared_round": n }).to_string(), 0),
        None => (String::new(), 0),
    }
}

/// `fno-agents review-summary` entry: prints the line or nothing, exit 0
/// either way, when branch+head name a head to display. A parse failure is
/// deliberate silence (print nothing, exit 0). `--evidence` replaces that
/// contract with the one-round-trip read: see `evidence_response`.
pub fn run_review_summary(args: &[String]) -> i32 {
    let Ok(parsed) = crate::cli_args::ReviewSummaryArgs::try_parse_from(args) else {
        return 0;
    };
    let events_path = parsed.events.clone().unwrap_or_else(default_events_path);
    if parsed.evidence {
        let mut input = String::new();
        let read_ok = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_ok();
        let (out, code) = evidence_response(
            &events_path,
            if read_ok { Some(input.as_str()) } else { None },
        );
        println!("{out}");
        return code;
    }
    if parsed.declared_round {
        let (Some(branch), Some(head), Some(flags)) = (&parsed.branch, &parsed.head, &parsed.flags)
        else {
            return 0;
        };
        let (out, code) = declared_round_response(&events_path, flags, branch, head);
        if !out.is_empty() {
            println!("{out}");
        }
        return code;
    }
    // SQL authority: the store's committed rows are the ledger; a missing or
    // unreadable store is the same deliberate silence a missing file was.
    let events_text = match crate::loopcheck::event_lines(&events_path) {
        Ok(lines) => lines.join("\n"),
        Err(_) => return 0,
    };
    if let (Some(branch), Some(head)) = (&parsed.branch, &parsed.head) {
        if let Some(line) = summary_line(&events_text, branch, head) {
            println!("{line}");
        }
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
    fn a_clean_latest_pass_pinned_to_the_asked_head_or_nothing() {
        // One contract, three arms: a fail verdict, a pass on another head,
        // and another branch's pass all print nothing. The display claim can
        // only ride the branch's latest pass at the asked head.
        let fail = format!(
            "{}\n{}\n",
            attestation("feature/x", "aaa1111", "pass", Some(1), 0),
            attestation("feature/x", "bbb2222", "fail", Some(2), 1),
        );
        assert_eq!(summary_line(&fail, "feature/x", "bbb2222"), None);
        let other_head = format!(
            "{}\n",
            attestation("feature/x", "aaa1111", "pass", Some(1), 0),
        );
        assert_eq!(summary_line(&other_head, "feature/x", "bbb2222"), None);
        let other_branch = format!(
            "{}\n{}\n",
            attestation("feature/other", "aaa1111", "pass", Some(1), 5),
            attestation("feature/x", "bbb2222", "pass", Some(1), 0),
        );
        assert_eq!(
            summary_line(&other_branch, "feature/x", "bbb2222"),
            Some("Reviewed at bbb2222: 1 rounds, 0 findings disposed.".to_string())
        );
    }

    #[test]
    fn degraded_invocations_stay_deliberately_silent_at_exit_zero() {
        let missing_file = run_review_summary(&[
            "--events".to_string(),
            "/nonexistent/fno-review-summary-test/events.jsonl".to_string(),
            "--branch".to_string(),
            "feature/x".to_string(),
            "--head".to_string(),
            "abc1234".to_string(),
        ]);
        assert_eq!(missing_file, 0);
        assert_eq!(run_review_summary(&[]), 0);
        assert_eq!(
            run_review_summary(&["--events".to_string(), "e.jsonl".to_string()]),
            0
        );
    }

    #[test]
    fn omitted_events_defaults_to_the_global_state_root_journal() {
        assert_eq!(
            default_events_path(),
            crate::scratch::fno_state_root().join("events.jsonl")
        );
    }

    #[test]
    fn evidence_splits_clean_from_missing() {
        let events = format!(
            "{}\n{}\n",
            attestation("feature/x-1111", "aaa1111", "pass", Some(1), 2),
            attestation("feature/x-2222", "bbb2222", "pass", Some(1), 0),
        );
        let items: Vec<Value> = serde_json::from_str(
            r#"[{"node":"x-1111","pr_number":1},{"node":"x-2222","pr_number":2},{"node":"x-3333","pr_number":3},{"node":null,"pr_number":null},{"node":"x-4444","pr_number":null}]"#,
        )
        .expect("items parse");
        let out = evidence(&events, &items);
        let states: Vec<&str> = out["items"]
            .as_array()
            .expect("items is an array")
            .iter()
            .map(|i| i["state"].as_str().expect("state is a string"))
            .collect();
        assert_eq!(
            states,
            ["scored", "clean", "no_attestation", "no_node", "no_pr"]
        );
        assert_eq!(
            out["evidence_line"],
            "evidence: scored=1 clean=1 missing=3 (no_node=1 no_pr=1 no_attestation=1)"
        );
        assert_eq!(out["items"][0]["finding_precision"], "pass");
        assert_eq!(out["items"][1]["finding_precision"], Value::Null);
    }

    #[test]
    fn evidence_classifies_both_branch_shapes_to_the_node() {
        let events = format!(
            "{}\n{}\n",
            attestation("bugfix/x-aaaa-wrong-close", "aaa1111", "pass", Some(1), 0),
            attestation("feature/x-aaaa", "bbb2222", "pass", Some(1), 0),
        );
        let items: Vec<Value> =
            serde_json::from_str(r#"[{"node":"x-aaaa","pr_number":1}]"#).expect("parse");
        let out = evidence(&events, &items);
        assert_eq!(out["items"][0]["state"], "clean");
        assert_eq!(
            out["evidence_line"],
            "evidence: scored=0 clean=1 missing=0 (no_node=0 no_pr=0 no_attestation=0)"
        );
    }

    #[test]
    fn evidence_scores_against_the_latest_verdict() {
        let events = format!(
            "{}\n{}\n",
            attestation("feature/x-4444", "aaa1111", "pass", Some(1), 0),
            attestation("feature/x-4444", "bbb2222", "fail", Some(2), 1),
        );
        let items: Vec<Value> =
            serde_json::from_str(r#"[{"node":"x-4444","pr_number":1}]"#).expect("parse");
        let out = evidence(&events, &items);
        assert_eq!(out["items"][0]["state"], "scored");
        // Precision follows the LATEST row; the earlier pass is history.
        assert_eq!(out["items"][0]["finding_precision"], "fail");
    }

    #[test]
    fn evidence_mode_refuses_non_array_stdin_with_exit_2() {
        let (out, code) = evidence_response(
            Path::new("/nonexistent/fno-evidence-test/events.jsonl"),
            Some("not json"),
        );
        assert_eq!(code, 2);
        assert!(out.contains("\"error\""));
        // Never prints a state for any item.
        assert!(!out.contains("\"state\""));
    }

    #[test]
    fn evidence_mode_refuses_an_unreadable_ledger() {
        let (out, code) = evidence_response(
            Path::new("/nonexistent/fno-evidence-test/events.jsonl"),
            Some("[]"),
        );
        assert_eq!(code, 2);
        assert!(out.contains("\"error\""));
    }

    #[test]
    fn verify_fixes_flags_name_the_declared_round() {
        // The declared round wins as the running max; an empty chain still
        // names round 1, floored exactly like review-hold round.
        let events = format!(
            "{}\n{}\n",
            attestation("feature/x", "aaa1111", "fail", Some(1), 2),
            attestation("feature/x", "bbb2222", "pass", Some(2), 0),
        );
        assert_eq!(
            declared_round(
                r#"["--verify-fixes","--comment"]"#,
                &events,
                "feature/x",
                "bbb2222"
            ),
            Some(2)
        );
        assert_eq!(
            declared_round(r#"["--verify-fixes"]"#, "", "feature/x", "bbb2222"),
            Some(1)
        );
    }

    #[test]
    fn flags_without_verify_fixes_stamp_nothing() {
        // Undeclared and malformed flag sets are the same None arm: a full
        // pass IS a round, and an unreadable flags payload stamps nothing.
        let events = attestation("feature/x", "aaa1111", "fail", Some(1), 2);
        assert_eq!(
            declared_round(r#"["--comment"]"#, &events, "feature/x", "aaa1111"),
            None
        );
        assert_eq!(declared_round("--verify-fixes", "", "feature/x", "h"), None);
    }

    #[test]
    fn declared_round_response_arbitrates_all_three_inputs() {
        let dir = std::env::temp_dir().join("fno-declared-round-response-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let events = dir.join("events.jsonl");
        std::fs::write(
            &events,
            format!(
                "{}\n",
                attestation("feature/x", "aaa1111", "fail", Some(1), 2)
            ),
        )
        .expect("write events");
        // A declared verify at the attested head prints the receipt.
        let (out, code) =
            declared_round_response(&events, r#"["--verify-fixes"]"#, "feature/x", "aaa1111");
        assert_eq!(code, 0);
        assert_eq!(out, r#"{"declared_round":1}"#);
        // Any other flag set prints nothing at exit 0.
        let (out, code) =
            declared_round_response(&events, r#"["--comment"]"#, "feature/x", "aaa1111");
        assert_eq!((out.as_str(), code), ("", 0));
        // An unreadable ledger is the same deliberate silence.
        let (out, code) = declared_round_response(
            Path::new("/nonexistent/fno-declared-round-test/events.jsonl"),
            r#"["--verify-fixes"]"#,
            "feature/x",
            "aaa1111",
        );
        assert_eq!((out.as_str(), code), ("", 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
