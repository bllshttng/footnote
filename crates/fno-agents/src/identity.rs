//! Harness-session address rules shared by every Rust producer and resolver.

/// The generated canonical handle: the harness's own short-id (the first eight
/// of the session id). A mail address is this short-id OR the full session id;
/// on a short-id collision, resolution fails closed and asks for the full id.
/// (Codex ids are time-prefixed, so their first-8 collides across same-window
/// sessions; codex addressing is often the full id in practice.)
///
/// Parity with Python `fno.harness_identity.canonical_handle` is load-bearing:
/// the Rust lifecycle client cannot import Python, and if the two rules differ a
/// durable send can address one handle while its recipient drains another and
/// silently strands on the bus.
pub(crate) fn canonical_handle(session_id: &str) -> String {
    let head: String = session_id.chars().take(8).collect();
    if session_id.starts_with("ses_") {
        head
    } else {
        head.to_ascii_lowercase()
    }
}

/// The retired last-eight address, read-only lookup compatibility only. Mail
/// addressed before the 2026-08-10 flip back to first-8 (when last-8 was the
/// address) still drains via this tier; it is never generated for new mail.
pub(crate) fn legacy_suffix_handle(session_id: &str) -> String {
    let mut tail = session_id.chars().rev().take(8).collect::<Vec<_>>();
    tail.reverse();
    let tail: String = tail.into_iter().collect();
    if session_id.starts_with("ses_") {
        tail
    } else {
        tail.to_ascii_lowercase()
    }
}

/// Full id, canonical (first-8), and retired suffix (last-8) tiers shared across
/// Rust paths. Tier 1 is the canonical address; tier 2 is the read-only
/// transition lookup.
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
        legacy_suffix_handle(session_id),
    ]
    .iter()
    .position(|value| equal(value))
    .map(|tier| tier as u8)
}

#[cfg(test)]
mod tests {
    use super::{canonical_handle, legacy_suffix_handle};

    #[test]
    fn canonical_is_first_eight_and_legacy_suffix_is_last_eight() {
        // UUID family: lowercased. canonical = first-8, legacy_suffix = last-8.
        assert_eq!(
            canonical_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4"),
            "019f48e1"
        );
        assert_eq!(
            legacy_suffix_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4"),
            "4bcf4ae4"
        );
        // OpenCode ses_ family: case preserved (canonical includes the prefix;
        // prefer the full id for ses_ short addressing).
        assert_eq!(canonical_handle("ses_7f3a9b2cAbCd1234"), "ses_7f3a");
        assert_eq!(legacy_suffix_handle("ses_7f3a9b2cAbCd1234"), "AbCd1234");
    }
}
