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
    /// "open", "fixed-unreviewed", "declined-uncorroborated",
    /// "declined-without-reason", or "truncated-remainder".
    pub axis: &'static str,
    /// A CONFIRMED correctness or security finding (or the truncated
    /// remainder, which cannot be inspected). At the round cap only a hard
    /// blocker withholds the merge; the rest are filed as nodes.
    pub hard: bool,
}

/// The two categories the round cap can never file away: a finding the
/// reviewer CONFIRMED as a correctness or security defect. The class gate is
/// what makes file-the-remainder safe - noise can be filed, a confirmed bug
/// cannot - so this reads the same primitive fields `gate_finding_blocks`
/// re-derives from, never the producer's count.
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

/// Whether the blockers still withhold `reviewed`. Under the budget every
/// blocker withholds. At the cap only a hard one does: the rest are filed by
/// the merge gate and the PR merges (one review stays the floor - an empty
/// chain never reaches here with rounds spent).
pub fn blockers_withhold(blockers: &[DispositionBlocker], rounds_exhausted: bool) -> bool {
    blockers.iter().any(|b| b.hard || !rounds_exhausted)
}

/// The IMPOSSIBLE predicate: rounds spent AND a hard blocker remains.
pub fn blockers_impossible(blockers: &[DispositionBlocker], rounds_exhausted: bool) -> bool {
    rounds_exhausted && blockers.iter().any(|b| b.hard)
}

/// The non-terminal blocking findings in an already-collected attestation
/// chain (the caller that parsed the events once passes the same chain to
/// every predicate).
///
/// Terminal means: fixed (and a LATER round reviewed the fix delta),
/// non-blocking by the gate's own re-derivation, or declined WITH
/// corroboration the author cannot mint alone (`self_attested_alone` is the
/// coverage row's existing predicate - a disposition pass carries its own
/// corroboration requirement, independent of
/// `config.review.require_corroboration`, because a disposition pass can be
/// gamed by declining and a clean review cannot). Pure: no IO. An empty chain
/// has no findings and blocks nothing.
pub fn disposition_blockers_on_chain(
    chain: &[Value],
    self_attested_alone: bool,
) -> Vec<DispositionBlocker> {
    if chain.is_empty() {
        return Vec::new();
    }
    let last_round = chain.len().saturating_sub(1);

    let mut findings: Vec<(String, &Value, usize, String)> = Vec::new(); // key, primitive, raised round, raised head
    let mut dispositions: std::collections::HashMap<String, (&str, &str, String)> =
        std::collections::HashMap::new(); // key -> (disposition, reason, disposing head)
    let mut truncated = false;
    for (index, val) in chain.iter().enumerate() {
        if val
            .pointer("/data/findings_truncated")
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            truncated = true;
        }
        let row_head = val
            .pointer("/data/head_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
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
                        slot.3 = row_head.clone();
                    } else {
                        findings.push((key.to_string(), primitive, index, row_head.clone()));
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
                dispositions.insert(key.to_string(), (disposition, reason, row_head.clone()));
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
            Some(("fixed", _, disposed_head)) => {
                // Terminal only when a LATER round reviewed the fix delta
                // AND attested a different head: a same-head re-run is one
                // more row under the same branch invocation with no new
                // commit, so the index alone would let an author clear a
                // CONFIRMED finding by re-attesting the head that raised
                // it. Equal heads refuse, empty included.
                if *raised >= last_round || disposed_head == raised_head {
                    blockers.push(DispositionBlocker {
                        finding_key: key.clone(),
                        axis: "fixed-unreviewed",
                        hard: hard_finding(primitive),
                    });
                }
            }
            Some(("declined", reason, _)) => {
                if reason.trim().is_empty() {
                    blockers.push(DispositionBlocker {
                        finding_key: key.clone(),
                        axis: "declined-without-reason",
                        hard: hard_finding(primitive),
                    });
                } else if self_attested_alone {
                    blockers.push(DispositionBlocker {
                        finding_key: key.clone(),
                        axis: "declined-uncorroborated",
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
    use super::disposition_blockers_on_chain;

    #[test]
    fn same_head_rerun_never_clears_a_fixed_finding() {
        // A fixed disposition is terminal only when the disposing round
        // attested a DIFFERENT head. A same-head re-run is one more row
        // under the same branch invocation with no new commit; keyed on the
        // round index alone, an author clears a CONFIRMED finding by
        // re-attesting the head that raised it.
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
        let blockers = disposition_blockers_on_chain(&chain, false);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].axis, "fixed-unreviewed");

        // A different head on the disposing round reviews the fix delta.
        let chain = vec![
            row("h1", "fail", vec![finding], vec![]),
            row("h2", "pass", vec![], vec![fixed_disp]),
        ];
        assert!(disposition_blockers_on_chain(&chain, false).is_empty());
    }
}
