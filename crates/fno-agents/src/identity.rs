//! Harness-session address rules shared by every Rust producer and resolver.

/// The generated canonical handle: the random tail of the harness session id.
pub(crate) fn canonical_handle(session_id: &str) -> String {
    let mut tail = session_id.chars().rev().take(8).collect::<Vec<_>>();
    tail.reverse();
    tail.into_iter().collect()
}

/// The retired first-eight address retained only for compatibility resolution.
pub(crate) fn legacy_prefix_handle(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// Full id, canonical tail, and retired prefix tiers shared across Rust paths.
pub(crate) fn session_handle_tier(token: &str, session_id: &str) -> Option<u8> {
    let token = token.trim();
    if token.is_empty() || session_id.is_empty() {
        return None;
    }
    let exact_case = session_id.starts_with("ses_");
    let equal = |value: &str| {
        if exact_case {
            token == value
        } else {
            token.eq_ignore_ascii_case(value)
        }
    };
    [
        session_id.to_string(),
        canonical_handle(session_id),
        legacy_prefix_handle(session_id),
    ]
    .iter()
    .position(|value| equal(value))
    .map(|tier| tier as u8)
}
