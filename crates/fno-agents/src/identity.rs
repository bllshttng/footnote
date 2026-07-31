//! Harness-session address rules shared by every Rust producer and resolver.

/// The generated canonical handle: the random tail of the harness session id.
pub(crate) fn canonical_handle(session_id: &str) -> String {
    let mut tail = session_id.chars().rev().take(8).collect::<Vec<_>>();
    tail.reverse();
    let tail: String = tail.into_iter().collect();
    if session_id.starts_with("ses_") {
        tail
    } else {
        tail.to_ascii_lowercase()
    }
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

#[cfg(test)]
mod tests {
    use super::canonical_handle;

    #[test]
    fn canonical_handle_normalizes_uuid_case_but_preserves_opencode_case() {
        assert_eq!(
            canonical_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4"),
            "4bcf4ae4"
        );
        assert_eq!(canonical_handle("ses_7f3a9b2cAbCd1234"), "AbCd1234");
    }
}
