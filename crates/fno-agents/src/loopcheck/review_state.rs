//! The coverage row's one-word answer: reviewed, refused, or unreviewed. A
//! module named by its question: `loopcheck.rs` is far over the file budget
//! and shrink-only, so the state deriver lives here with the tests that pin
//! it.
//!
//! The predicate that gives the answer its politics: a refusal names the
//! state only when the verdict was OWED. An optional GitHub App is honored
//! when it responds and owed nothing when it does not, so its quota bounce
//! must not rename an uncovered row after a reviewer nobody was waiting on.
//! Both derivations (this one and `_derive_review_state` in
//! `cli/src/fno/pr/_reviews.py`) are pinned against the same golden table,
//! `cli/tests/config/review_state_table.json`.

use super::*;

impl CoverageReport {
    pub fn review_state(&self) -> Option<ReviewState> {
        if matches!(self.coverage, Coverage::Unknown) {
            return None;
        }
        if self.verdicts.iter().any(|verdict| {
            verdict.verdict == CoverageVerdict::Reviewed
                && human_approval_counts(verdict, self.github_approval_satisfies)
        }) {
            return Some(ReviewState::Reviewed);
        }
        if self
            .verdicts
            .iter()
            .any(|verdict| verdict.verdict == CoverageVerdict::Refused && verdict.required)
        {
            return Some(ReviewState::ReviewerRefused);
        }
        Some(ReviewState::Unreviewed)
    }

    pub fn refused_reviewers(&self) -> Vec<&str> {
        self.verdicts
            .iter()
            .filter(|verdict| verdict.verdict == CoverageVerdict::Refused)
            .map(|verdict| verdict.name.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopcheck::{classify_coverage, Freshness};

    fn github_verdict_with_required(
        name: &str,
        verdict: CoverageVerdict,
        required: bool,
    ) -> ReviewerVerdict {
        ReviewerVerdict {
            producer: CoverageProducer::GithubApp,
            name: name.to_string(),
            verdict,
            human_approval: false,
            author_approval: false,
            attestation_origin: AttestationOrigin::Unknown,
            reviewed_sha: String::new(),
            freshness: None,
            scope: None,
            refusal_reason: None,
            reviewer_context: None,
            required,
            passed: false,
        }
    }

    #[test]
    fn optional_app_refusal_does_not_name_the_review_state() {
        // The defect this bit closes: an optional App's quota bounce renamed
        // an uncovered PR `reviewer_refused`, and `awaiting_review_only` read
        // that label as a wait for a reviewer nobody was owed. The positive
        // marker: the state is Unreviewed - the honest "nothing has reviewed
        // this head" - which routes the worker to the runnable local remedy.
        let rep = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: vec![github_verdict_with_required(
                "chatgpt-codex-connector",
                CoverageVerdict::Refused,
                false,
            )],
        };
        assert_eq!(rep.review_state(), Some(ReviewState::Unreviewed));
    }

    #[test]
    fn required_app_refusal_still_names_the_review_state() {
        let rep = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: vec![github_verdict_with_required(
                "chatgpt-codex-connector",
                CoverageVerdict::Refused,
                true,
            )],
        };
        assert_eq!(rep.review_state(), Some(ReviewState::ReviewerRefused));
        assert!(rep.refused_reviewers().contains(&"chatgpt-codex-connector"));
    }

    #[test]
    fn local_attestation_refusal_is_always_owed() {
        // A local refusal records an attempt that RAN and declined - real
        // work with a real remedy - so it keeps naming itself. The wire form
        // carries NO `required` key: true is omitted on serialize, which is
        // byte-identical to what every pre-field row looked like.
        let events = r#"{"type":"review_invocation","data":{"stage":"refused","head_sha":"abc12345","verb":"code-review","reason":"empty_diff"}}"#;
        let rep = classify_coverage(
            &[],
            &[],
            events,
            &[],
            true,
            None,
            &|_| Freshness::Fresh,
            "",
            "abc12345",
        );
        let verdict = rep.verdicts.first().expect("the refused verdict");
        assert_eq!(verdict.verdict, CoverageVerdict::Refused);
        assert!(verdict.required);
        let wire = serde_json::to_value(verdict).unwrap();
        assert!(wire.get("required").is_none(), "{wire}");
        assert_eq!(rep.review_state(), Some(ReviewState::ReviewerRefused));
    }

    #[test]
    fn a_row_with_no_required_key_reads_required() {
        // The deserialization half of the default: a stored verdict emitted
        // before the field existed carries no `required` key, and reading it
        // back must keep today's semantics - the refusal was owed.
        let stored = serde_json::json!({
            "producer": "github_app",
            "name": "chatgpt-codex-connector",
            "verdict": "refused"
        });
        assert!(stored.get("required").is_none());
        let verdict: ReviewerVerdict = serde_json::from_value(stored).unwrap();
        assert!(verdict.required, "absence reads REQUIRED, never skipped");
        let rep = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Covered(0),
            verdicts: vec![verdict],
        };
        assert_eq!(rep.review_state(), Some(ReviewState::ReviewerRefused));
    }

    #[test]
    fn review_state_table_rows_match_between_both_legs() {
        // One oracle, two readers (the optional_apps_default.json pattern):
        // `CoverageReport::review_state` here and `_derive_review_state` in
        // cli/src/fno/pr/_reviews.py both answer these rows, so a drift on
        // either side fails its own test against the SAME file.
        let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cli/tests/config/review_state_table.json");
        let text = std::fs::read_to_string(&golden)
            .unwrap_or_else(|e| panic!("read golden {}: {e}", golden.display()));
        let table: serde_json::Value = serde_json::from_str(&text).unwrap();
        for row in table["rows"].as_array().expect("rows") {
            let verdicts: Vec<ReviewerVerdict> = row["verdicts"]
                .as_array()
                .expect("verdicts")
                .iter()
                .map(|v| {
                    serde_json::from_value(v.clone())
                        .unwrap_or_else(|e| panic!("row {}: {e}", row["name"]))
                })
                .collect();
            let rep = CoverageReport {
                github_approval_satisfies: false,
                coverage: Coverage::Covered(0),
                verdicts,
            };
            let expected = match row["expected_state"].as_str().unwrap() {
                "reviewed" => ReviewState::Reviewed,
                "reviewer_refused" => ReviewState::ReviewerRefused,
                "unreviewed" => ReviewState::Unreviewed,
                other => panic!("unknown expected_state {other}"),
            };
            assert_eq!(
                rep.review_state(),
                Some(expected),
                "row {} diverged",
                row["name"].as_str().unwrap_or("?")
            );
        }
    }
}
