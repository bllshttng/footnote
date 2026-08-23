use fno_agents::loopcheck::TerminationReason;
use fno_agents::run_outcome::{classify, legacy_projection};

#[test]
fn run_outcome_matches_every_legacy_terminal_predicate() {
    let reasons = [
        TerminationReason::DonePRGreen,
        TerminationReason::DoneAdvisory,
        TerminationReason::DoneDelivery,
        TerminationReason::DoneBatched,
        TerminationReason::DoneAwaitingMerge,
        TerminationReason::DoneUnreviewed,
        TerminationReason::DoneAwaitingReview,
        TerminationReason::DonePlanned,
        TerminationReason::NoWork,
        TerminationReason::Budget,
        TerminationReason::NoProgress,
        TerminationReason::Interrupted,
        TerminationReason::Aborted,
    ];

    for reason in reasons {
        let expected = legacy_projection(&reason);
        let actual = classify(reason.clone()).projection();
        assert_eq!(actual, expected, "classification drift for {reason:?}");
    }
}

#[test]
fn advisory_graduates_but_planned_does_not() {
    assert!(
        classify(TerminationReason::DoneAdvisory)
            .projection()
            .graduate
    );
    assert!(
        !classify(TerminationReason::DonePlanned)
            .projection()
            .graduate
    );
}
