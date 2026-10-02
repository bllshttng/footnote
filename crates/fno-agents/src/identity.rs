//! Harness-session address rules shared by every Rust producer and resolver.

/// Footnote's one session-id mint: a random v4 UUID in 8-4-4-4-12 form.
/// Errors when the OS gives no randomness; there is no clock fallback.
pub fn mint_fno_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("could not mint an fno_id: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// fno's own 8-hex handle for a registry row: the head of the row's
/// fno-minted `fno_id`. None for a legacy row whose fno_id is a harness
/// copy, a short id or a name - those keep the harness-head address.
pub(crate) fn fno_handle(fno_id: Option<&str>, own_ids: &[&str]) -> Option<String> {
    let id = fno_id?;
    let groups: Vec<&str> = id.split('-').collect();
    let v4 = groups.len() == 5
        && [
            groups[0].len(),
            groups[1].len(),
            groups[2].len(),
            groups[3].len(),
            groups[4].len(),
        ] == [8, 4, 4, 4, 12]
        && groups
            .iter()
            .all(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_hexdigit()))
        && groups[2].as_bytes()[0] == b'4';
    if !v4 || own_ids.iter().any(|own| own.eq_ignore_ascii_case(id)) {
        return None;
    }
    Some(id[..8].to_ascii_lowercase())
}

/// mint_fno_id, redrawn until its 8-hex head is not in `taken` (lowercase
/// heads). Errors after 64 draws rather than looping.
pub fn mint_unique_fno_id(taken: &std::collections::HashSet<String>) -> Result<String, String> {
    mint_unique_fno_id_with(taken, mint_fno_id)
}

fn mint_unique_fno_id_with(
    taken: &std::collections::HashSet<String>,
    mut mint: impl FnMut() -> Result<String, String>,
) -> Result<String, String> {
    for _ in 0..64 {
        let id = mint()?;
        if !taken.contains(&id[..8]) {
            return Ok(id);
        }
    }
    Err("could not mint a unique fno_id head after 64 draws".to_string())
}

/// The harness head (the first eight of the session id), kept as a read-only
/// tier so handles printed before fno minted row handles keep resolving, and
/// kept as the mail bus key. It is no longer the row address: a row's address
/// is [`fno_handle`], the head of the id fno itself mints. Codex ids are
/// time-prefixed, so their first-8 collides across same-window sessions; a
/// shared head still resolves while it names one row and refuses naming
/// every match when it names two.
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

/// The agent handle this process resolves as, or None when the caller is no
/// agent session: the ancestry prover's session id through the canonical
/// handle. One read shared by the law door and the clear door, so the two
/// never disagree about who is at the door.
pub(crate) fn ambient_agent_handle() -> Option<String> {
    let ident = crate::spawn_context::resolve_self_identity(
        &|name| std::env::var(name).ok(),
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let session_id = ident.session_id.as_deref()?;
    ident.harness.as_deref()?;
    let handle = canonical_handle(session_id);
    (!handle.is_empty()).then_some(handle)
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

/// Whether an id's shape can definitively name `harness`. Only claude
/// (UUIDv4), codex (time-prefixed UUIDv7), and opencode (`ses_`) ids carry a
/// known shape; grok and pi threads run fno-minted UUIDv4s under their own
/// harness, so a v4 shape alone cannot convict them of being claude.
pub(crate) fn shape_known_harness(harness: &str) -> bool {
    matches!(harness, "claude" | "codex" | "opencode")
}

/// The harness an id's own shape names, or None when the shape is silent.
/// Parity with Python `fno.harness_identity.harness_of_session_id`: the
/// keeper re-validates under the lock, so the two rules must agree or one
/// side accepts a stamp the other refuses.
pub(crate) fn harness_of_session_id(session_id: &str) -> Option<&'static str> {
    let sid = session_id.trim();
    if sid.is_empty() {
        return None;
    }
    let bytes = sid.as_bytes();
    if sid.starts_with("ses_") && bytes[4..].iter().all(|b| b.is_ascii_alphanumeric()) {
        return Some("opencode");
    }
    // 8-4-4-4-12 hex; the version nibble is the first hex of group three.
    let groups: Vec<&str> = sid.split('-').collect();
    if groups.len() == 5
        && [
            groups[0].len(),
            groups[1].len(),
            groups[2].len(),
            groups[3].len(),
            groups[4].len(),
        ] == [8, 4, 4, 4, 12]
        && groups
            .iter()
            .all(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        let version = groups[2].as_bytes()[0].to_ascii_lowercase();
        return match version {
            b'4' => Some("claude"),
            b'7' => Some("codex"),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use proptest::prelude::*;

    use super::{canonical_handle, legacy_suffix_handle, mint_fno_id, mint_unique_fno_id_with};

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
    fn mint_fno_id_is_well_formed_v4_and_unique() {
        let re = regex::Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .unwrap();
        let a = mint_fno_id().expect("mint succeeds when the OS has randomness");
        assert!(re.is_match(&a), "not a v4 UUID: {a}");
        assert_ne!(a, mint_fno_id().unwrap(), "two mints must differ");
        // The unique mint: a fresh head passes, and a stub that keeps landing
        // on a taken head errors after 64 draws instead of looping.
        let taken: std::collections::HashSet<String> =
            std::iter::once(a[..8].to_string()).collect();
        assert!(mint_unique_fno_id_with(&taken, || Ok(a.clone())).is_err());
        assert!(
            mint_unique_fno_id_with(&std::collections::HashSet::new(), || Ok(a.clone())).is_ok()
        );
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
