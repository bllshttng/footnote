use super::*;

/// AC5-HP: enums parse known gh strings.
#[test]
fn pr_state_parses_known_gh_strings() {
    assert_eq!(PrState::from_gh_str("OPEN"), PrState::Open);
    assert_eq!(PrState::from_gh_str("MERGED"), PrState::Merged);
    assert_eq!(PrState::from_gh_str("CLOSED"), PrState::Closed);
    assert_eq!(PrState::from_gh_str("none"), PrState::None);
}

/// AC5-EDGE: an unexpected gh state string maps to PrState::None
/// (fail-closed), never panics.
#[test]
fn pr_state_unknown_string_fails_closed() {
    assert_eq!(PrState::from_gh_str("DRAFT"), PrState::None);
    assert_eq!(PrState::from_gh_str(""), PrState::None);
    assert_eq!(PrState::from_gh_str("open"), PrState::None);
}

/// AC5-UI: as_str/render reproduce the exact legacy fingerprint vocabulary.
#[test]
fn enum_rendering_byte_identical_to_legacy_strings() {
    assert_eq!(PrState::Open.as_str(), "OPEN");
    assert_eq!(PrState::Merged.as_str(), "MERGED");
    assert_eq!(PrState::Closed.as_str(), "CLOSED");
    assert_eq!(PrState::None.as_str(), "none");
    assert_eq!(CiConclusion::Success.render(), "SUCCESS");
    assert_eq!(
        CiConclusion::Failure(Some("lint".into())).render(),
        "FAILURE:lint"
    );
    assert_eq!(CiConclusion::Failure(None).render(), "FAILURE");
    assert_eq!(CiConclusion::Pending.render(), "PENDING");
    assert_eq!(CiConclusion::Skipped.render(), "skipped");
    assert_eq!(CiConclusion::None.render(), "none");
}
