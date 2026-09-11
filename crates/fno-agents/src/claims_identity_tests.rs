// Identity-tag tests, moved verbatim from claims.rs (shrink-only budget).
use super::*;

// ---- harness tag (x-3e70) ---------------------------------------------

// AC6-FR: a claim record written before this change (no `harness` key)
// parses with `harness: None` and does not crash.
#[test]
fn claim_without_harness_key_reads_none() {
    let yaml = "schema_version: 1\nkey: node:x\nholder: h\nacquired_at: 1\npid: 2\nhost: hh\n";
    let rec = parse_claim_str(yaml).expect("legacy record must parse");
    assert_eq!(rec.harness, None);
}

// A record WITH a harness key round-trips it back.
#[test]
fn claim_with_harness_key_round_trips() {
    let yaml = "schema_version: 1\nkey: node:x\nholder: h\nacquired_at: 1\npid: 2\nhost: hh\nharness: codex\n";
    let rec = parse_claim_str(yaml).expect("record must parse");
    assert_eq!(rec.harness.as_deref(), Some("codex"));
    // None is omitted from output entirely (not serialized as null).
    let none = ClaimRecord {
        harness: None,
        ..rec.clone()
    };
    assert!(!serialize_claim(&none).unwrap().contains("harness"));
}

// ---- session_id tag ----------------------------------------------------

// AC2: a claim record written before this change (no `session_id` key)
// parses with `session_id: None` and does not crash.
#[test]
fn claim_without_session_id_key_reads_none() {
    let yaml = "schema_version: 1\nkey: node:x\nholder: h\nacquired_at: 1\npid: 2\nhost: hh\n";
    let rec = parse_claim_str(yaml).expect("legacy record must parse");
    assert_eq!(rec.session_id, None);
}

// A record WITH a session_id key round-trips it back, and None omits the
// key entirely (not serialized as null).
#[test]
fn claim_with_session_id_key_round_trips() {
    let yaml = "schema_version: 1\nkey: node:x\nholder: h\nacquired_at: 1\npid: 2\nhost: hh\nsession_id: abc123\n";
    let rec = parse_claim_str(yaml).expect("record must parse");
    assert_eq!(rec.session_id.as_deref(), Some("abc123"));
    let none = ClaimRecord {
        session_id: None,
        ..rec.clone()
    };
    assert!(!serialize_claim(&none).unwrap().contains("session_id"));
}

// AC1: resolve_identity resolves session_id and harness from one call, so
// make_claim can never stamp a record naming a session of a different
// harness than the one it names.
#[test]
fn resolve_identity_resolves_session_and_harness_together() {
    let get = |key: &str| match key {
        "FNO_HARNESS_NAME" => Some("claude".to_string()),
        "FNO_HARNESS_SESSION_ID" => Some("abc123".to_string()),
        _ => None,
    };
    assert_eq!(
        resolve_identity_from(get),
        (Some("abc123".to_string()), Some("claude".to_string()))
    );
}

#[test]
fn resolve_harness_single_family_wins_disagreement_is_unknown() {
    // Two DISAGREEING families are ambiguous: precedence must not pick codex
    // and tag the claim with a harness this process cannot prove it owns.
    let both = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("cx".to_string()),
        "CLAUDE_CODE_SESSION_ID" => Some("cl".to_string()),
        _ => None,
    };
    assert_eq!(resolve_harness_from(both).as_deref(), None);
    // Two markers of ONE family carrying the SAME id -> that family.
    let same_family = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("cx".to_string()),
        "CODEX_SESSION_ID" => Some("CX".to_string()),
        _ => None,
    };
    assert_eq!(resolve_harness_from(same_family).as_deref(), Some("codex"));
    // Two DIFFERENT ids of ONE family disagree: attribute nothing, never
    // the table-first value (x-0992) - without proof either id could be
    // the stranger's.
    let same_family_disagree = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("cx".to_string()),
        "CODEX_SESSION_ID" => Some("cx2".to_string()),
        _ => None,
    };
    assert_eq!(resolve_harness_from(same_family_disagree).as_deref(), None);
    // A blank higher-precedence marker is UNSET; a lower real one still wins.
    let blank_hi = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("   ".to_string()),
        "CLAUDE_CODE_SESSION_ID" => Some("cl".to_string()),
        _ => None,
    };
    assert_eq!(resolve_harness_from(blank_hi).as_deref(), Some("claude"));
    assert_eq!(
        resolve_harness_from(|k| (k == "OPENCODE_SESSION_ID").then(|| "ses_1".to_string()))
            .as_deref(),
        Some("opencode")
    );
    // No markers -> None (unknown), never a panic.
    assert_eq!(resolve_harness_from(|_| None), None);
}

#[test]
fn canonical_pair_resolves_harness_and_parent_session_before_vendor_markers() {
    let session = "11111111-1111-4111-8111-111111111111";
    let get = |key: &str| match key {
        "FNO_HARNESS_NAME" => Some("claude".to_string()),
        "FNO_HARNESS_SESSION_ID" => Some(session.to_string()),
        _ => None,
    };

    assert_eq!(resolve_harness_from(&get).as_deref(), Some("claude"));
    assert_eq!(ambient_parent_edge_from(&get).0.as_deref(), Some(session));
    assert_eq!(ambient_parent_edge_from(get).1.as_deref(), Some("claude"));
}

#[test]
fn canonical_partial_and_vendor_contradictions_refuse() {
    assert_eq!(
        resolve_harness_from(|key| {
            (key == "FNO_HARNESS_SESSION_ID").then(|| "session-only".to_string())
        }),
        None
    );
    assert_eq!(
        resolve_harness_from(|key| match key {
            "FNO_HARNESS_NAME" => Some("claude".to_string()),
            "FNO_HARNESS_SESSION_ID" => Some("claude-session".to_string()),
            "CODEX_THREAD_ID" => Some("codex-session".to_string()),
            _ => None,
        }),
        None
    );
}

#[test]
fn command_stamp_scrubs_all_identity_and_sets_only_bound_session() {
    let mut command = std::process::Command::new("sh");
    command.env("CODEX_THREAD_ID", "parent");
    command.env("FNO_AGENT_SELF", "parent");
    command.env("FNO_AGENT_SUBSTRATE", "pane");
    command.env(FNO_HARNESS_NAME, "parent");
    stamp_command_env(
        &mut command,
        Some("child"),
        "claude",
        Some(" 11111111-1111-4111-8111-111111111111 "),
    );
    let values: std::collections::HashMap<_, _> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();

    assert_eq!(
        values.get("FNO_AGENT_SELF").map(String::as_str),
        Some("child")
    );
    assert_eq!(
        values.get("FNO_AGENT_HARNESS").map(String::as_str),
        Some("claude")
    );
    assert_eq!(
        values.get(FNO_HARNESS_NAME).map(String::as_str),
        Some("claude")
    );
    assert_eq!(
        values.get(FNO_HARNESS_SESSION_ID).map(String::as_str),
        Some("11111111-1111-4111-8111-111111111111")
    );
    assert!(!values.contains_key("CODEX_THREAD_ID"));
    // These ask lanes stamp no substrate of their own, so the parent's must
    // go: a child that keeps `pane` reads as attended for life (x-be78).
    assert!(!values.contains_key("FNO_AGENT_SUBSTRATE"));

    let mut unowned = std::process::Command::new("sh");
    unowned.env("FNO_AGENT_SELF", "parent");
    stamp_command_env(&mut unowned, None, "codex", None);
    let values: std::collections::HashMap<_, _> = unowned
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    assert!(!values.contains_key("FNO_AGENT_SELF"));
}

#[test]
fn ambient_parent_edge_resolves_id_harness_and_refuses_mixing() {
    // Single claude marker: the VALUE is the parent session id.
    let claude = |k: &str| match k {
        "CLAUDE_CODE_SESSION_ID" => Some("7420e8f7-eeba".to_string()),
        _ => None,
    };
    assert_eq!(
        ambient_parent_edge_from(claude),
        (
            Some("7420e8f7-eeba".to_string()),
            Some("claude".to_string()),
            ambient_parent_edge_from(|_| None).2
        )
    );
    // Within the codex family one id under both names resolves to it.
    // Two DIFFERENT ids of the family disagree and attribute nothing
    // (x-0992) - the durable thread id used to win by position, which
    // laundered whichever marker sorted first into the parent record.
    let codex = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("t-1".to_string()),
        "CODEX_SESSION_ID" => Some("t-1".to_string()),
        _ => None,
    };
    assert_eq!(
        ambient_parent_edge_from(codex),
        (
            Some("t-1".to_string()),
            Some("codex".to_string()),
            ambient_parent_edge_from(|_| None).2
        )
    );
    let codex_disagree = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("t-1".to_string()),
        "CODEX_SESSION_ID" => Some("legacy-1".to_string()),
        _ => None,
    };
    assert_eq!(
        ambient_parent_edge_from(codex_disagree),
        (None, None, ambient_parent_edge_from(|_| None).2)
    );
    // Two DISAGREEING families attribute NOTHING: no laundered lineage.
    let mixed = |k: &str| match k {
        "CODEX_THREAD_ID" => Some("cx".to_string()),
        "CLAUDE_CODE_SESSION_ID" => Some("cl".to_string()),
        _ => None,
    };
    let (s, h, c) = ambient_parent_edge_from(mixed);
    assert_eq!((s, h), (None, None));
    assert!(c.is_some(), "cwd is captured even when identity is refused");
    // No markers: identity absent, cwd still present, never a panic.
    let (s, h, _) = ambient_parent_edge_from(|_| None);
    assert_eq!((s, h), (None, None));
}
