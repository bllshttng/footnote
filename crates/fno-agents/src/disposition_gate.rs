//! The disposition-complete pass condition, Rust gate side.
//!
//! Locked Decision 1: a head is covered when the chain tiles AND every
//! finding in that chain is TERMINAL. Locked Decision 6: the gate re-derives
//! blocking from the per-finding primitives with its own copy of the rule and
//! never trusts the producer's `findings_blocking` count.
//!
//! The chain itself is collected by [`crate::loopcheck::in_scope_chain`]; the
//! predicates here answer one question: which blocking findings in that chain
//! are non-terminal?

use serde_json::Value;

/// The gate's own copy of the harmless-category allowlist, held equal to
/// `DEFAULT_NONBLOCKING_CATEGORIES` on the Python side by tests (Locked
/// Decision 6: two implementations, one corpus).
const GATE_NONBLOCKING_CATEGORIES: &[&str] = &[
    "style",
    "formatting",
    "naming",
    "docs",
    "typo",
    "nit",
    "simplification",
    "test-coverage",
];

fn gate_finding_blocks(primitive: &Value) -> bool {
    if primitive
        .get("has_required_fields")
        .and_then(|v| v.as_bool())
        != Some(true)
    {
        return true;
    }
    let verdict = primitive
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if verdict.trim().eq_ignore_ascii_case("confirmed") {
        return true;
    }
    let category = primitive
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let lowered = category.trim().to_lowercase();
    !GATE_NONBLOCKING_CATEGORIES.contains(&lowered.as_str())
}

/// One non-terminal blocking finding, named by its finding_key plus the axis
/// that failed, for the block message and the emitted row.
#[derive(Debug, Clone, PartialEq)]
pub struct DispositionBlocker {
    pub finding_key: String,
    /// "open", "fixed-unreviewed", "declined-without-reason", or
    /// "truncated-remainder".
    pub axis: &'static str,
    /// A CONFIRMED correctness or security finding (or the truncated
    /// remainder, which cannot be inspected). Recorded on the receipt and
    /// gates nothing: at the configured rounds the budget discharges every
    /// open finding, hard included.
    pub hard: bool,
}

/// The two categories a finding the reviewer CONFIRMED as a correctness or
/// security defect can carry. Recorded on the blocker for the receipt; it
/// reads the same primitive fields `gate_finding_blocks` re-derives from,
/// never the producer's count.
const HARD_CATEGORIES: &[&str] = &["correctness", "security"];

fn hard_finding(primitive: &Value) -> bool {
    let verdict = primitive
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !verdict.trim().eq_ignore_ascii_case("confirmed") {
        return false;
    }
    let category = primitive
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    HARD_CATEGORIES.contains(&category.as_str())
}

/// Whether the blockers still withhold `reviewed`: any open finding
/// withholds below the cap, and at the cap nothing does - the configured
/// rounds are the whole review gate, so the PR merges on green CI.
pub fn blockers_withhold(blockers: &[DispositionBlocker], rounds_exhausted: bool) -> bool {
    !rounds_exhausted && !blockers.is_empty()
}

/// The head an attestation row pinned, for the delta-witness read.
fn row_head(row: &Value) -> &str {
    row.pointer("/data/head_sha")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// The non-terminal blocking findings in an already-collected attestation
/// chain (the caller that parsed the events once passes the same chain to
/// every predicate).
///
/// Terminal means: fixed (and a LATER round reviewed a head the finding's
/// last raise did not sit on), non-blocking by the gate's own re-derivation,
/// or declined WITH a recorded reason. Origin never gates: whoever attested
/// the disposition, the terminality is the same. Pure: no IO. An empty chain
/// has no findings and blocks nothing.
pub fn disposition_blockers_on_chain(chain: &[Value]) -> Vec<DispositionBlocker> {
    if chain.is_empty() {
        return Vec::new();
    }

    let mut findings: Vec<(String, &Value, usize, String)> = Vec::new(); // key, primitive, raised round, raised head
    let mut dispositions: std::collections::HashMap<String, (&str, &str)> =
        std::collections::HashMap::new(); // key -> (disposition, reason)
    let mut truncated = false;
    for (index, val) in chain.iter().enumerate() {
        if val
            .pointer("/data/findings_truncated")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            truncated = true;
        }
        let head = row_head(val).to_string();
        for primitive in val
            .pointer("/data/findings")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            if let Some(key) = primitive.get("finding_key").and_then(|v| v.as_str()) {
                if !key.is_empty() {
                    if let Some(slot) = findings.iter_mut().find(|(k, _, _, _)| k == key) {
                        slot.1 = primitive;
                        slot.2 = index;
                        slot.3 = head.clone();
                    } else {
                        findings.push((key.to_string(), primitive, index, head.clone()));
                    }
                }
            }
        }
        for entry in val
            .pointer("/data/dispositions")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            let key = entry
                .get("finding_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let disposition = entry
                .get("disposition")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let reason = entry.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            if !key.is_empty() {
                dispositions.insert(key.to_string(), (disposition, reason));
            }
        }
    }

    let mut blockers: Vec<DispositionBlocker> = Vec::new();
    if truncated {
        blockers.push(DispositionBlocker {
            finding_key: "(truncated remainder)".to_string(),
            axis: "truncated-remainder",
            hard: true,
        });
    }
    for (key, primitive, raised, raised_head) in &findings {
        if !gate_finding_blocks(primitive) {
            continue; // non-blocking by class: no action clears the gate
        }
        match dispositions.get(key) {
            None => blockers.push(DispositionBlocker {
                finding_key: key.clone(),
                axis: "open",
                hard: hard_finding(primitive),
            }),
            Some(("fixed", _)) => {
                // Terminal when a LATER round reviewed the fix delta. The
                // delta witness reads the rows AFTER the last raise, not
                // the disposition entry: the disposition rides one event,
                // so a last-wins copy of its head pinned the gate to a
                // stale same-head dispose forever when the real fix's
                // round re-emitted nothing.
                let delta_witnessed = chain[*raised + 1..]
                    .iter()
                    .any(|row| row_head(row) != raised_head.as_str());
                if !delta_witnessed {
                    blockers.push(DispositionBlocker {
                        finding_key: key.clone(),
                        axis: "fixed-unreviewed",
                        hard: hard_finding(primitive),
                    });
                }
            }
            Some(("declined", reason)) => {
                if reason.trim().is_empty() {
                    blockers.push(DispositionBlocker {
                        finding_key: key.clone(),
                        axis: "declined-without-reason",
                        hard: hard_finding(primitive),
                    });
                }
            }
            // A "nonblocking" disposition over a gate-blocking finding: the
            // gate's re-derivation wins (Locked Decision 6).
            Some(_) => blockers.push(DispositionBlocker {
                finding_key: key.clone(),
                axis: "open",
                hard: hard_finding(primitive),
            }),
        }
    }
    blockers
}

#[cfg(test)]
mod tests {
    use super::{blockers_withhold, disposition_blockers_on_chain};

    #[test]
    fn same_head_rerun_never_clears_a_fixed_finding() {
        // A fixed disposition is terminal only when a later round attested a
        // head the finding's last raise did not sit on. A same-head re-run is
        // one more row under the same branch invocation with no new commit;
        // keyed on the round index alone, an author clears a CONFIRMED
        // finding by re-attesting the head that raised it.
        use serde_json::json;
        let finding = json!({
            "finding_key": "f.py:1:correctness",
            "category": "correctness",
            "verdict": "CONFIRMED",
            "has_required_fields": true,
        });
        let row = |head: &str,
                   verdict: &str,
                   findings: Vec<serde_json::Value>,
                   dispositions: Vec<serde_json::Value>| {
            json!({
                "type": "review_attestation",
                "data": {
                    "head_sha": head,
                    "verdict": verdict,
                    "branch": "feature/x",
                    "findings": findings,
                    "dispositions": dispositions,
                }
            })
        };
        let fixed_disp = json!({
            "finding_key": "f.py:1:correctness",
            "disposition": "fixed",
            "reason": "re-attested at the same head",
        });
        // Same head, two rounds: the clearance must NOT read terminal.
        let chain = vec![
            row("h1", "fail", vec![finding.clone()], vec![]),
            row("h1", "pass", vec![], vec![fixed_disp.clone()]),
        ];
        let blockers = disposition_blockers_on_chain(&chain);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].axis, "fixed-unreviewed");

        // A different head on the disposing round reviews the fix delta.
        let chain = vec![
            row("h1", "fail", vec![finding], vec![]),
            row("h2", "pass", vec![], vec![fixed_disp]),
        ];
        assert!(disposition_blockers_on_chain(&chain).is_empty());
    }

    #[test]
    fn attest_empty_commit_dispose_is_terminal() {
        // Attest, empty-commit, dispose: the new head between raise and
        // dispose IS the fix-delta witness, and origin never gates - the
        // disposition is terminal on the author's own signature alone.
        use serde_json::json;
        let finding = json!({
            "finding_key": "f.py:1:correctness",
            "category": "correctness",
            "verdict": "CONFIRMED",
            "has_required_fields": true,
        });
        let chain = vec![
            json!({"type": "review_attestation", "data": {
                "head_sha": "h1", "verdict": "fail", "branch": "feature/x",
                "findings": [finding], "dispositions": [],
            }}),
            json!({"type": "review_attestation", "data": {
                "head_sha": "h2", "verdict": "pass", "branch": "feature/x",
                "findings": [],
                "dispositions": [{"finding_key": "f.py:1:correctness",
                                  "disposition": "fixed",
                                  "reason": "attested by the author"}],
            }}),
        ];
        assert!(disposition_blockers_on_chain(&chain).is_empty());
    }

    #[test]
    fn a_declined_finding_with_a_reason_is_terminal() {
        // Origin never gates: the author's own decline with a recorded
        // reason reads exactly like a second session's.
        use serde_json::json;
        let finding = json!({
            "finding_key": "s.rs:1:security",
            "category": "security",
            "verdict": null,
            "blocking": true,
            "has_required_fields": true,
        });
        let chain = vec![json!({"type": "review_attestation", "data": {
            "head_sha": "h1", "verdict": "fail", "branch": "feature/x",
            "findings": [finding],
            "dispositions": [{"finding_key": "s.rs:1:security",
                              "disposition": "declined",
                              "reason": "not applicable here"}],
        }})];
        assert!(disposition_blockers_on_chain(&chain).is_empty());

        // A reason-less decline stays non-terminal.
        let chain = vec![json!({"type": "review_attestation", "data": {
            "head_sha": "h1", "verdict": "fail", "branch": "feature/x",
            "findings": [finding],
            "dispositions": [{"finding_key": "s.rs:1:security",
                              "disposition": "declined",
                              "reason": "  "}],
        }})];
        assert_eq!(disposition_blockers_on_chain(&chain).len(), 1);
    }

    #[test]
    fn real_fix_after_a_stale_same_head_dispose_answers() {
        // A same-head `fixed`, then a real fix whose round re-emits no
        // disposition: the delta witness reads the later rows, so the chain
        // is terminal instead of pinned to the stale same-head dispose
        // forever.
        use serde_json::json;
        let finding = json!({
            "finding_key": "f.py:1:correctness",
            "category": "correctness",
            "verdict": "CONFIRMED",
            "has_required_fields": true,
        });
        let chain = vec![
            json!({"type": "review_attestation", "data": {
                "head_sha": "h1", "verdict": "fail", "branch": "feature/x",
                "findings": [finding], "dispositions": [],
            }}),
            json!({"type": "review_attestation", "data": {
                "head_sha": "h1", "verdict": "pass", "branch": "feature/x",
                "findings": [],
                "dispositions": [{"finding_key": "f.py:1:correctness",
                                  "disposition": "fixed",
                                  "reason": "same-head dispose"}],
            }}),
            json!({"type": "review_attestation", "data": {
                "head_sha": "h2", "verdict": "pass", "branch": "feature/x",
                "findings": [], "dispositions": [],
            }}),
        ];
        assert!(disposition_blockers_on_chain(&chain).is_empty());
    }

    #[test]
    fn withhold_below_the_cap_any_open_at_the_cap_none() {
        // One predicate: below the configured rounds any open finding
        // withholds; at the cap nothing does, hard included.
        let open_hard = vec![super::DispositionBlocker {
            finding_key: "f.py:1:correctness".to_string(),
            axis: "open",
            hard: true,
        }];
        assert!(blockers_withhold(&open_hard, false));
        assert!(!blockers_withhold(&open_hard, true));
        assert!(!blockers_withhold(&[], false));
    }
}
