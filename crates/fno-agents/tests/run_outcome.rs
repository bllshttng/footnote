use fno_agents::loopcheck::TerminationReason;
use fno_agents::run_outcome::{classify, classify_legacy, legacy_projection};

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
fn every_wire_reason_round_trips_through_the_authoritative_record() {
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
        let wire = format!("{reason:?}");
        let record = classify(reason.clone());
        assert_eq!(classify_legacy(&wire).unwrap(), record);
        assert_eq!(TerminationReason::from(record), reason);
    }
}

#[test]
fn unknown_wire_reason_is_rejected() {
    assert!(classify_legacy("DoneTypo").is_err());
}

#[test]
fn delegated_finalize_compatibility_is_ledger_only() {
    let delegated = classify_legacy("delegated").unwrap().projection();
    assert!(delegated.record_ledger);
    assert!(!delegated.ship_reason);
    assert!(!delegated.stuck);
    assert!(!delegated.do_stamp_terminal);
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
