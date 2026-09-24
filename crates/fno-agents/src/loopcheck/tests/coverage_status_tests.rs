use super::*;

#[test]
fn the_published_status_never_names_a_harness_specific_verb() {
    // The shared commit status is read by every harness. A claude verb
    // baked in by a claude publisher is a command a codex worker does not
    // have. Measured: the live status on this very PR named the claude
    // verb at a concrete <level>, because a claude session published it.
    let d = uncovered_status_description("8411cde1aa2b0000000000000000000000000000", 1201);

    // The POSITIVE marker first: assert the neutral command IS present.
    // An absence-only test passes on an empty string, or on a render that
    // never ran, and would not notice this function returning "".
    assert!(
        d.contains("fno do target request-self-review --pr 1201"),
        "must name the harness-neutral command: {d}"
    );
    assert!(d.contains("8411cde1"), "must name the uncovered head: {d}");

    // Then the absence, pinned per harness so a future edit that reaches
    // for the publisher's own verb fails here rather than in a codex
    // worker's session.
    for verb in ["/code-review", "/review", "/fno:review", "--comment"] {
        assert!(
            !d.contains(verb),
            "harness-specific verb `{verb}` leaked into the shared status: {d}"
        );
    }

    // GitHub rejects an over-long description WHOLE, losing the marker
    // rather than truncating it, so the cap is load-bearing.
    assert!(
        d.len() <= 140,
        "over GitHub's 140-char cap ({}): {d}",
        d.len()
    );
}

#[test]
fn coverage_instrument_status_is_pending_only_when_the_read_is_unknown() {
    let unknown = coverage_instrument_status(&Coverage::Unknown, "abc123456789");
    assert_eq!(unknown.0, "pending");
    assert!(unknown.1.contains("coverage read unavailable"));
    assert!(unknown.1.contains("retry the review verb"));

    let known = coverage_instrument_status(&Coverage::Covered(0), "abc123456789");
    assert_eq!(known.0, "success");
    assert!(known.1.contains("coverage read healthy"));
}
