use fno_agents::loopcheck::TerminationReason;

#[test]
fn generic_completion_preserves_legacy_terminal_serialization() {
    assert_eq!(
        serde_json::to_string(&TerminationReason::DonePRGreen).unwrap(),
        "\"DonePRGreen\""
    );
    assert_eq!(
        serde_json::to_string(&TerminationReason::DoneAdvisory).unwrap(),
        "\"DoneAdvisory\""
    );
}

#[test]
fn generic_completion_is_not_a_legacy_terminal_route_yet() {
    assert!(serde_json::from_str::<TerminationReason>("\"DoneDelivery\"").is_err());
    assert!(serde_json::from_str::<TerminationReason>("\"DoneGeneric\"").is_err());
}
