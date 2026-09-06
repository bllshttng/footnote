//! Lookup-law tests: the registry's primary key is the
//! `(harness, harness_session_id)` pair; a label - its own
//! name or a prior `aliases` entry - resolves, but an AMBIGUOUS label (two
//! rows answer it) resolves to nothing rather than to the first match.

use super::*;

/// One row, minimal, with the identity axes the lookups key on.
fn entry(name: &str, harness: &str, sid: Option<&str>) -> RegistryEntry {
    RegistryEntry {
        name: name.into(),
        aliases: Vec::new(),
        short_id: String::new(),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        cwd: "/tmp/x".into(),
        project_root: String::new(),
        session_id: None,
        claude_session_uuid: None,
        harness: Some(harness.into()),
        harness_session_id: sid.map(str::to_string),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        launch_account: None,
        related_session_id: None,
        node: None,
        requested_model: None,
        requested_provider: None,
        requested_effort: None,
        route_provider_id: None,
        model_name: None,
        account_record_id: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None,
        cc_session_id: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: "2026-09-04T00:00:00Z".into(),
        pid: None,
        pid_start_time: None,
        keeper_child_pid: None,
        substrate: None,
        log_path: None,
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        origin: None,
        spawn_trigger: None,
        liveness: None,
        liveness_measured_at: None,
        harness_title: None,
        launch_account_source: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        adopted_by_session: None,
        legacy_claude_short_id: None,
    }
}

#[test]
fn find_by_session_keys_on_the_pair_not_the_id_alone() {
    let mut reg = Registry::default();
    reg.entries = vec![
        entry("a", "claude", Some("11111111-1111-4111-8111-111111111111")),
        entry("b", "codex", Some("11111111-1111-4111-8111-111111111111")),
    ];
    // The same uuid on a different harness is a DIFFERENT row: the pair is
    // the key (codex uuid7 head-8 collides inside one minute).
    assert!(
        reg.find_by_session("claude", "11111111-1111-4111-8111-111111111111")
            .is_some_and(|e| e.name == "a"),
        "the claude pair resolves row a"
    );
    assert!(
        reg.find_by_session("opencode", "11111111-1111-4111-8111-111111111111")
            .is_none(),
        "no opencode row carries that id"
    );
}

#[test]
fn keyed_write_hits_only_the_resolved_row_ac1_hp() {
    // AC1-HP: two rows share a label; a writer that resolved ONE by session
    // id and writes under the lock changes only that row.
    let sid_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let sid_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let mut reg = Registry::default();
    reg.entries = vec![
        entry("dup", "claude", Some(sid_a)),
        entry("dup", "claude", Some(sid_b)),
    ];
    let key = ("claude".to_string(), Some(sid_b.to_string()));
    if let Some(e) = find_keyed_mut(&mut reg, &key, "dup") {
        e.last_message_at = Some("hit".into());
    }
    assert!(
        reg.find_by_session("claude", sid_a)
            .unwrap()
            .last_message_at
            .is_none(),
        "row a untouched"
    );
    assert_eq!(
        reg.find_by_session("claude", sid_b)
            .unwrap()
            .last_message_at
            .as_deref(),
        Some("hit"),
        "row b carries the write"
    );
    // And the ambiguous label resolves to nothing: find refuses rather than
    // handing the writer whichever row iterates first.
    assert!(reg.find("dup").is_none(), "ambiguous label refuses");
    assert!(reg.find_mut("dup").is_none());
    assert_eq!(reg.label_matches_count("dup"), 2, "both rows answer it");
}

#[test]
fn demoted_find_resolves_prior_labels_then_session_ids() {
    let sid = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let mut e = entry("new-name", "claude", Some(sid));
    e.aliases = vec!["old-name".into()];
    let reg = Registry {
        schema_version: 1,
        entries: vec![e],
    };
    assert!(reg.find("new-name").is_some(), "own name resolves");
    assert!(reg.find("old-name").is_some(), "a prior label resolves");
    assert!(
        reg.find(sid).is_some(),
        "the full session id resolves (identity tier)"
    );
    assert!(reg.find("nope").is_none());
}

#[test]
fn related_and_predecessor_ids_resolve_at_the_full_tier() {
    let related = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let pred = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let mut e = entry(
        "row",
        "claude",
        Some("ffffffff-ffff-4fff-8fff-ffffffffffff"),
    );
    e.related_session_id = Some(related.into());
    e.predecessor_session_ids = vec![pred.into()];
    let reg = Registry {
        schema_version: 1,
        entries: vec![e],
    };
    assert!(reg.find(related).is_some(), "a parked fork id resolves");
    assert!(reg.find(pred).is_some(), "a predecessor id resolves");
}
