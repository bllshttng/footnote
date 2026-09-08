//! The merge gate's self lane against the shipped default posture. A bare
//! install resolves `self_review` with components `[self]`, and the self
//! component asks ONE question: did someone review this head? Which producer
//! recorded the verdict is origin, and origin never gates. The old predicate
//! demanded `LocalAttestation`, so a bare install refused a PR carrying an
//! independent GitHub App review and cleared on nothing but a self-attestation.
use super::*;

fn report(verdicts: Vec<ReviewerVerdict>) -> CoverageReport {
    CoverageReport {
        github_approval_satisfies: false,
        coverage: Coverage::Covered(verdicts.len()),
        verdicts,
    }
}

fn verdict(producer: CoverageProducer, name: &str, verdict: CoverageVerdict) -> ReviewerVerdict {
    ReviewerVerdict {
        producer,
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
        required: false,
        passed: false,
    }
}

fn default_self_review_posture() -> PostureConfig {
    resolve_posture_config(&Settings::default())
}

#[test]
fn github_app_review_satisfies_the_default_self_lane() {
    let config = default_self_review_posture();
    assert_eq!(config.value, "self_review");
    let rep = report(vec![verdict(
        CoverageProducer::GithubApp,
        "chatgpt-codex-connector",
        CoverageVerdict::Reviewed,
    )]);
    let v = posture_verdict(&config, &rep, &[]);
    assert!(
        v.posture_satisfied,
        "an independent review must clear the default self lane, got gaps: {:?}",
        v.posture_gaps
    );
}

#[test]
fn declare_still_fails_the_self_lane() {
    let config = default_self_review_posture();
    for producer in [
        CoverageProducer::LocalAttestation,
        CoverageProducer::GithubApp,
    ] {
        let rep = report(vec![verdict(
            producer,
            "declare",
            CoverageVerdict::Reviewed,
        )]);
        let v = posture_verdict(&config, &rep, &[]);
        assert!(
            !v.posture_satisfied,
            "declare must satisfy no rung on any producer"
        );
        assert!(
            v.posture_gaps.iter().any(|g| g.starts_with("self:")),
            "the self gap must be named, got: {:?}",
            v.posture_gaps
        );
    }
}

#[test]
fn app_verdicts_that_are_not_reviewed_fail_the_self_lane() {
    let config = default_self_review_posture();
    for kind in [
        CoverageVerdict::Refused,
        CoverageVerdict::Absent,
        CoverageVerdict::Stale,
        CoverageVerdict::Errored,
    ] {
        let rep = report(vec![verdict(
            CoverageProducer::GithubApp,
            "chatgpt-codex-connector",
            kind.clone(),
        )]);
        let v = posture_verdict(&config, &rep, &[]);
        assert!(
            !v.posture_satisfied,
            "only a Reviewed verdict counts, got satisfied on {kind:?}: {:?}",
            v.posture_gaps
        );
    }
}

#[test]
fn bare_settings_still_resolve_the_self_review_rung() {
    let config = resolve_posture_config(&Settings::default());
    assert_eq!(config.value, "self_review");
    assert_eq!(config.source, "default");
    assert_eq!(config.components, &["self"]);
}
