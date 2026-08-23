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
    use std::path::PathBuf;
    use std::process::Command;

    use proptest::prelude::*;

    use super::{canonical_handle, legacy_suffix_handle};

    fn session_id_strategy() -> impl Strategy<Value = Vec<String>> {
        let uuid_lower_pattern = [
            r"[0-9a-f]{8}",
            r"[0-9a-f]{4}",
            r"[0-9a-f]{4}",
            r"[0-9a-f]{4}",
            r"[0-9a-f]{12}",
        ]
        .join("-");
        let uuid_upper_pattern = [
            r"[0-9A-F]{8}",
            r"[0-9A-F]{4}",
            r"[0-9A-F]{4}",
            r"[0-9A-F]{4}",
            r"[0-9A-F]{12}",
        ]
        .join("-");
        let uuid_v7_pattern = [
            r"[0-9a-f]{8}",
            r"[0-9a-f]{4}",
            r"7[0-9a-f]{3}",
            r"[89ab][0-9a-f]{3}",
            r"[0-9a-f]{12}",
        ]
        .join("-");
        let uuid_lower = proptest::string::string_regex(&uuid_lower_pattern).unwrap();
        let uuid_upper = proptest::string::string_regex(&uuid_upper_pattern).unwrap();
        let uuid_v7 = proptest::string::string_regex(&uuid_v7_pattern).unwrap();
        let opencode = proptest::string::string_regex(r"ses_[A-Za-z0-9]{8,32}").unwrap();
        let short_hex = proptest::string::string_regex(r"[0-9a-f]{4,8}").unwrap();
        (uuid_lower, uuid_upper, uuid_v7, opencode, short_hex)
            .prop_map(|(lower, upper, v7, opencode, short)| vec![lower, upper, v7, opencode, short])
    }

    fn python_canonical_handles(session_ids: &[String]) -> Vec<String> {
        let cli_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cli/src");
        let input = serde_json::to_string(session_ids).expect("session ids serialize");
        let output = Command::new("python3")
            .args([
                "-c",
                "import json, sys; from fno.harness_identity import canonical_handle; print(json.dumps([canonical_handle(value) for value in json.loads(sys.argv[1])]))",
                &input,
            ])
            .env("PYTHONPATH", cli_src)
            .output()
            .expect("python3 is required for Rust/Python identity parity");
        assert!(
            output.status.success(),
            "Python canonical_handle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("Python canonical_handle returned JSON")
    }

    #[test]
    fn legacy_suffix_is_last_eight_and_preserves_opencode_case() {
        // UUID family: lowercased. legacy_suffix = last-8.
        assert_eq!(
            legacy_suffix_handle("019F48E1-5B09-72A0-9BC8-6B364BCF4AE4"),
            "4bcf4ae4"
        );
        // OpenCode ses_ family: case preserved.
        assert_eq!(legacy_suffix_handle("ses_7f3a9b2cAbCd1234"), "AbCd1234");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn canonical_handle_matches_python_across_id_families(session_ids in session_id_strategy()) {
            let expected = python_canonical_handles(&session_ids);
            let actual: Vec<_> = session_ids.iter().map(|id| canonical_handle(id)).collect();
            prop_assert_eq!(actual, expected);
        }
    }
}
