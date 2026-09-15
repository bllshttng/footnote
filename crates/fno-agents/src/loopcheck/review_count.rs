//! How many reviews a coverage row reports. A round is a review whatever it
//! concluded, so the count never reads below the rounds the chain already
//! spent. `loopcheck.rs` is far over the file budget and shrink-only, so the
//! count lives here with the tests that pin it.

use super::*;

pub(super) fn reviewed_count(rep: &CoverageReport, tiling: Option<&RangeTiling>) -> usize {
    let verdicts = match rep.coverage {
        Coverage::Covered(n) => n,
        Coverage::Unknown => rep
            .verdicts
            .iter()
            .filter(|v| {
                v.verdict == CoverageVerdict::Reviewed
                    && human_approval_counts(v, rep.github_approval_satisfies)
            })
            .count(),
    };
    let rounds = tiling.map_or(0, |t| usize::try_from(t.rounds_used).unwrap_or(0));
    verdicts.max(rounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopcheck::{classify_coverage, coverage_event_data_tiled, Freshness};

    #[test]
    fn a_fail_round_reads_as_one_review_beside_zero_verdicts() {
        // The measured shape: no verdict counts, but the chain spent a round,
        // so the row must not read "zero reviews" beside rounds_used 1.
        let rep = classify_coverage(
            &[],
            &[],
            "",
            &[],
            true,
            None,
            &|_| Freshness::Fresh,
            "",
            "h",
        );
        assert_eq!(rep.coverage, Coverage::Covered(0));
        let tiling = RangeTiling {
            rounds_used: 1,
            ..Default::default()
        };
        let data = coverage_event_data_tiled(826, &rep, "h", "", None, Some(&tiling));
        assert_eq!(data["coverage"], serde_json::json!("uncovered"));
        assert_eq!(data["reviewed_count"], serde_json::json!(1));
        assert_eq!(data["passed_count"], serde_json::json!(0));
    }

    #[test]
    fn no_tiling_and_zero_rounds_keep_the_zero_count() {
        let rep = classify_coverage(
            &[],
            &[],
            "",
            &[],
            true,
            None,
            &|_| Freshness::Fresh,
            "",
            "h",
        );
        let none = coverage_event_data_tiled(826, &rep, "h", "", None, None);
        assert_eq!(none["reviewed_count"], serde_json::json!(0));
        let tiling = RangeTiling {
            rounds_used: 0,
            ..Default::default()
        };
        let zero = coverage_event_data_tiled(826, &rep, "h", "", None, Some(&tiling));
        assert_eq!(zero["reviewed_count"], serde_json::json!(0));
    }

    #[test]
    fn the_max_never_lowers_a_two_reviewer_count() {
        let events = format!(
            "{}\n{}",
            serde_json::json!({
                "type": "review_attestation",
                "data": {"reviewer": "code-review", "head_sha": "h", "verdict": "pass"}
            }),
            serde_json::json!({
                "type": "review_attestation",
                "data": {"reviewer": "peer-review", "head_sha": "h", "verdict": "pass"}
            })
        );
        let rep = classify_coverage(
            &[],
            &[],
            &events,
            &[],
            true,
            None,
            &|_| Freshness::Fresh,
            "",
            "h",
        );
        assert_eq!(rep.coverage, Coverage::Covered(2));
        let tiling = RangeTiling {
            rounds_used: 1,
            ..Default::default()
        };
        let data = coverage_event_data_tiled(826, &rep, "h", "", None, Some(&tiling));
        assert_eq!(data["reviewed_count"], serde_json::json!(2));
    }

    #[test]
    fn an_unknown_row_counts_a_reviewed_local_verdict() {
        // The comment beside the unknown arm promises the honest measured
        // count; a reviewed verdict behind an unmeasurable coverage word is
        // one review, not silence that reads as nobody-reviewed.
        let rep = CoverageReport {
            github_approval_satisfies: false,
            coverage: Coverage::Unknown,
            verdicts: vec![ReviewerVerdict {
                producer: CoverageProducer::LocalAttestation,
                name: "code-review".to_string(),
                verdict: CoverageVerdict::Reviewed,
                human_approval: false,
                author_approval: false,
                attestation_origin: AttestationOrigin::Unknown,
                reviewed_sha: "h".to_string(),
                freshness: Some(Freshness::Fresh),
                scope: None,
                refusal_reason: None,
                reviewer_context: None,
                required: true,
                passed: false,
            }],
        };
        let data = coverage_event_data_tiled(826, &rep, "h", "", None, None);
        assert_eq!(data["coverage"], serde_json::json!("unknown"));
        assert_eq!(data["reviewed_count"], serde_json::json!(1));
    }
}
