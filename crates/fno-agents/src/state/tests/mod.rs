//! State-layer unit tests. Moved verbatim out of `state.rs`, which is over
//! the 5,000-line budget and therefore shrink-only; test motion is the
//! sanctioned shrink. The `#[path]` child below keeps loading from
//! `src/state/tests/`, exactly as the inline module did.

use super::*;

/// Tests that mutate process-wide env vars hold this so parallel test
/// threads never interleave mid-arm snapshots of the same keys.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn enters_fires_once_per_episode() {
    use InsideLegState::{Blocked, Done, Working};
    // Walk working -> blocked -> blocked -> blocked -> working -> blocked.
    // `enters(.., Blocked)` must be true ONLY on the two edges into blocked
    // (positions 2 and 6), not on the repeats within an episode.
    let seq = [Working, Blocked, Blocked, Blocked, Working, Blocked];
    let fired: Vec<bool> = seq
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let prev = if i == 0 { None } else { Some(seq[i - 1]) };
            enters(prev, s, Blocked)
        })
        .collect();
    assert_eq!(fired, [false, true, false, false, false, true]);
    // A first-ever report of blocked (prev None) counts as entering.
    assert!(enters(None, Blocked, Blocked));
    // Done is its own episode axis, independent of blocked.
    assert!(enters(Some(Working), Done, Done));
    assert!(!enters(Some(Done), Done, Done));
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "fno-agents-state-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn sample_entry(name: &str) -> RegistryEntry {
    RegistryEntry {
        substrate: None,
        node: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        launch_account: None,
        related_session_id: None,
        name: name.into(),
        short_id: format!("{name}-id"),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: Some("codex".into()),
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        route_provider_id: None,
        model_name: None,
        account_record_id: None,
        cwd: "/tmp/x".into(),
        project_root: "/tmp/x".into(),
        session_id: Some("uuid-1".into()),
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: Some("uuid-1".into()),
        gemini_session_id: None,
        mcp_channel_id: None,
        host_mode: None,
        cc_session_id: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: "2026-05-24T00:00:00Z".into(),
        pid: Some(1234),
        pid_start_time: None,
        keeper_child_pid: None,
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
        legacy_claude_short_id: None,
        ..Default::default()
    }
}

#[test]
fn state_substrate_roundtrips_and_absence_reads_none() {
    // v23: the lane stamp survives the FILE round-trip through the same
    // load/save pair the daemon uses, and a row without one reads None -
    // absence means unknown, never "pane".
    let dir = tmpdir("substrate");
    let path = dir.join("registry.json");
    let mut stamped = sample_entry("thread-worker");
    stamped.substrate = Some("thread".into());
    let mut old_row = sample_entry("old-row");
    // Distinct session identity: the write path backfills
    // harness_session_id from codex_session_id, and two rows sharing
    // "uuid-1" refuse the store on an identity collision.
    old_row.codex_session_id = Some("uuid-2".into());
    update_registry(&path, |r| {
        r.entries.push(stamped);
        r.entries.push(old_row);
    })
    .unwrap();
    let reg = load_registry(&path).unwrap();
    let thread_row = reg
        .entries
        .iter()
        .find(|e| e.name == "thread-worker")
        .unwrap();
    assert_eq!(thread_row.substrate.as_deref(), Some("thread"));
    let old_row = reg.entries.iter().find(|e| e.name == "old-row").unwrap();
    assert_eq!(old_row.substrate, None);

    // skip-when-None keeps an unstamped row's key off disk entirely.
    let raw = std::fs::read_to_string(&path).unwrap();
    let old_obj = raw.split("old-row").nth(1).unwrap();
    assert!(!old_obj.contains("substrate"));

    // Python-authored shape (the key present) parses into the same value.
    let python_row = r#"{"name":"m","provider":"claude","cwd":"/p","log_path":null,
            "created_at":"2026-07-02T00:00:00Z","status":"live",
            "substrate":"headless"}"#;
    let row: RegistryEntry = serde_json::from_str(python_row).unwrap();
    assert_eq!(row.substrate.as_deref(), Some("headless"));
}

#[test]
fn state_mux_ref_roundtrips_and_python_dict_shape_parses() {
    // 4a-G2: the mux ref survives the typed round-trip, and the exact
    // JSON shape Python's AgentEntry writes ({"session": ..., "pane_id":
    // ...} under "mux") parses back into MuxRef (X3 mixed-language rule).
    let mut e = sample_entry("mux-agent");
    e.short_id = String::new(); // one live ref: mux only
    e.mux = Some(MuxRef {
        session: "work".into(),
        pane_id: 7,
    });
    let json = serde_json::to_string(&e).unwrap();
    let back: RegistryEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back.mux.as_ref().unwrap().session, "work");
    assert_eq!(back.mux.as_ref().unwrap().pane_id, 7);

    // Python-authored shape (dict passthrough) parses identically.
    let python_row = r#"{"name":"m","provider":"claude","cwd":"/p","log_path":null,
            "claude_short_id":null,"codex_session_id":null,"gemini_session_id":null,
            "created_at":"2026-07-02T00:00:00Z","status":"live","last_message_at":null,
            "mcp_channel_id":null,"mux":{"session":"main","pane_id":3}}"#;
    let row: RegistryEntry = serde_json::from_str(python_row).unwrap();
    assert_eq!(row.mux.as_ref().unwrap().pane_id, 3);
    // A pre-mux row (absent key) reads as None.
    assert_eq!(sample_entry("plain").mux, None);
}

#[test]
fn harness_backfill_legacy_row_gains_canonical() {
    // x-ec59 / AC1-EDGE: a pre-migration Python row (provider + the legacy
    // per-provider uuid, no harness) gains the canonical pair on load.
    let python_legacy = r#"{"name":"w","provider":"claude","cwd":"/p","log_path":null,
            "claude_short_id":"7c5dcf5d","claude_session_uuid":"UUID-1","codex_session_id":null,
            "gemini_session_id":null,"created_at":"2026-07-13T00:00:00Z","status":"live",
            "last_message_at":null,"mcp_channel_id":null}"#;
    let mut e: RegistryEntry = serde_json::from_str(python_legacy).unwrap();
    e.migrate_provider_semantics(14);
    e.backfill_harness_aliases();
    assert_eq!(e.harness.as_deref(), Some("claude"));
    assert_eq!(e.harness_session_id.as_deref(), Some("UUID-1"));
}

#[test]
fn backfill_short_id_moves_legacy_into_empty_short() {
    // AC2-EDGE (Rust side): a legacy row's claude_short_id moves into short_id.
    let legacy = r#"{"name":"w","provider":"claude","cwd":"/p","log_path":null,
            "claude_short_id":"7c5dcf5d","created_at":"2026-07-13T00:00:00Z","status":"live"}"#;
    let mut e: RegistryEntry = serde_json::from_str(legacy).unwrap();
    assert_eq!(e.backfill_short_id(), None);
    assert_eq!(e.short_id, "7c5dcf5d");
    assert_eq!(e.legacy_claude_short_id, None); // consumed
}

#[test]
fn backfill_short_id_conflict_keeps_short_and_reports_legacy() {
    // AC3-EDGE (Rust side): both set, different -> short_id wins, legacy surfaced.
    let conflict = r#"{"name":"w","provider":"claude","cwd":"/p","log_path":null,
            "short_id":"aaaaaaaa","claude_short_id":"bbbbbbbb",
            "created_at":"2026-07-13T00:00:00Z","status":"live"}"#;
    let mut e: RegistryEntry = serde_json::from_str(conflict).unwrap();
    assert_eq!(e.backfill_short_id().as_deref(), Some("bbbbbbbb"));
    assert_eq!(e.short_id, "aaaaaaaa"); // short_id wins
}

#[test]
fn harness_backfill_canonical_only_row_syncs_legacy() {
    // A canonical-only row (post-migration mint): the legacy alias is synced
    // so an old reader still resolves the session.
    let mut e = sample_entry("w");
    e.legacy_provider = "claude".into();
    e.codex_session_id = None;
    e.session_id = None;
    e.claude_session_uuid = None;
    e.harness = Some("claude".into());
    e.harness_session_id = Some("CANON".into());
    e.backfill_harness_aliases();
    assert_eq!(e.claude_session_uuid.as_deref(), Some("CANON"));
}

#[test]
fn harness_backfill_conflict_is_canonical_wins() {
    // AC2-EDGE (Rust side): a conflicting legacy value is overwritten.
    let mut e = sample_entry("w");
    e.legacy_provider = "claude".into();
    e.harness = Some("claude".into());
    e.harness_session_id = Some("CANON".into());
    e.claude_session_uuid = Some("STALE".into());
    e.backfill_harness_aliases();
    assert_eq!(e.harness_session_id.as_deref(), Some("CANON"));
    assert_eq!(e.claude_session_uuid.as_deref(), Some("CANON"));
}

#[test]
fn harness_backfill_does_not_cross_contaminate() {
    // A claude row carrying a stale codex id must NOT adopt it: only the
    // row's own harness key is consulted when harness is known.
    let mut e = sample_entry("w");
    e.legacy_provider = "claude".into();
    e.harness = Some("claude".into());
    e.harness_session_id = None;
    e.claude_session_uuid = None;
    e.codex_session_id = Some("STALE-CODEX".into());
    e.session_id = None;
    e.backfill_harness_aliases();
    assert_eq!(e.harness_session_id, None);
}

#[test]
fn harness_backfill_reads_python_canonical_row_via_registry() {
    // Cross-language: a Python-authored canonical codex row parses into
    // Registry and, after the load-time backfill (mirrors
    // read_registry_tolerant), resolves the legacy alias too.
    let python_json = r#"{"schema_version":7,"agents":[{"name":"w","provider":"codex",
            "cwd":"/p","log_path":null,"claude_short_id":null,"codex_session_id":null,
            "gemini_session_id":null,"created_at":"2026-07-13T00:00:00Z","status":"live",
            "last_message_at":null,"mcp_channel_id":null,"harness":"codex",
            "harness_session_id":"THREAD"}]}"#;
    let mut reg: Registry = serde_json::from_str(python_json).unwrap();
    for e in &mut reg.entries {
        e.backfill_harness_aliases();
    }
    assert_eq!(reg.entries[0].harness_session_id.as_deref(), Some("THREAD"));
    assert_eq!(reg.entries[0].codex_session_id.as_deref(), Some("THREAD"));
}

#[test]
fn state_mux_row_skips_key_when_absent() {
    // Slim rows: no "mux" key serialized for non-mux rows, so a
    // round-tripped worker row stays byte-familiar to older tooling.
    let v = serde_json::to_value(sample_entry("w")).unwrap();
    assert!(v.get("mux").is_none());
}

#[test]
fn state_mux_row_is_never_a_one_shot_ask() {
    // codex P1 (PR #142): empty short_id + no pid describes a mux row too;
    // reconcile must not settle a live hosted agent as a finished ask.
    let mut e = sample_entry("mux-live");
    e.short_id = String::new();
    e.pid = None;
    assert!(e.is_one_shot_ask(), "baseline: bare row reads as ask");
    e.mux = Some(MuxRef {
        session: "main".into(),
        pane_id: 4,
    });
    assert!(!e.is_one_shot_ask(), "a mux ref is a live hosting handle");
}

#[test]
fn state_v9_claude_shellout_row_is_a_one_shot_ask() {
    // x-1b1e regression: v9 moved the claude jobId into short_id, so a
    // finished claude `ask`/`--bg` row now carries a NON-empty short_id.
    // The empty-short_id proxy no longer catches it; without the provider+
    // host_mode guard reconcile would fall through to the reachability probe
    // and keep the row falsely `live` off its surviving (resumability-only)
    // session file -- the exact defect recover() already had to fix.
    let mut ask = sample_entry("cc-ask");
    ask.legacy_provider = "claude".into();
    ask.harness = None;
    ask.backfill_harness_aliases();
    ask.short_id = "7c5dcf5d".into(); // v9: jobId lives here now
    ask.host_mode = None; // exec (shellout), not interactive
    ask.pid = None;
    ask.mux = None;
    assert!(
        ask.is_one_shot_ask(),
        "a v9 claude shellout row (non-empty short_id, exec, no pid) is a one-shot ask"
    );

    // An interactive claude stream worker DOES have a daemon PTY: probe it,
    // never settle it by liveness-alone.
    let mut worker = ask.clone();
    worker.host_mode = Some(HOST_MODE_INTERACTIVE.into());
    assert!(
        !worker.is_one_shot_ask(),
        "an interactive claude worker is PTY-managed, not a one-shot ask"
    );

    // An adopted row carries an external pid -> excluded by the pid guard.
    let mut adopted = ask.clone();
    adopted.host_mode = Some(HOST_MODE_ATTACHED.into());
    adopted.pid = Some(4242);
    assert!(
        !adopted.is_one_shot_ask(),
        "an adopted row (external pid) is not a one-shot ask"
    );
}

#[test]
fn state_update_registry_enforces_one_live_ref() {
    // Write-time invariant (brief Locked 7): a mux ref alongside a worker
    // short_id (or a bg claude_short_id) is refused; the store is left
    // untouched and the lock released (a later clean write succeeds).
    let dir = tmpdir("one-ref");
    let path = dir.join("registry.json");
    let res = update_registry(&path, |r| {
        let mut e = sample_entry("double"); // sample has short_id set
        e.mux = Some(MuxRef {
            session: "main".into(),
            pane_id: 1,
        });
        r.entries.push(e);
    });
    assert!(
        matches!(res, Err(StateError::InvariantViolation(_))),
        "double-ref row must be refused: {res:?}"
    );
    assert!(
        load_registry(&path).unwrap().entries.is_empty(),
        "refused write must not persist"
    );
    // bg-thread ref (jobId in short_id, v9) + mux is refused the same way.
    let res = update_registry(&path, |r| {
        let mut e = sample_entry("bg-double");
        e.short_id = "abcd1234".into();
        e.mux = Some(MuxRef {
            session: "main".into(),
            pane_id: 2,
        });
        r.entries.push(e);
    });
    assert!(matches!(res, Err(StateError::InvariantViolation(_))));
    // A clean mux-only row persists (lock was released by the refusals).
    update_registry(&path, |r| {
        let mut e = sample_entry("clean");
        e.short_id = String::new();
        e.mux = Some(MuxRef {
            session: "main".into(),
            pane_id: 3,
        });
        r.entries.push(e);
    })
    .unwrap();
    let reg = load_registry(&path).unwrap();
    assert_eq!(reg.entries.len(), 1);
    assert_eq!(reg.schema_version, REGISTRY_SCHEMA_VERSION);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn state_update_registry_allows_first_eight_overlap_between_sessions() {
    let dir = tmpdir("identity-collision");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut first = sample_entry("first");
        first.short_id = "transport1".into();
        first.harness = Some("codex".into());
        first.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(first);
    })
    .unwrap();

    // A first-eight overlap between two DIFFERENT sessions is the codex
    // same-window shape (UUIDv7 ids share their top bits inside one
    // time window), not a write-blocking collision: resolution fails
    // closed on the shared short and the full id still resolves.
    update_registry(&path, |registry| {
        let mut second = sample_entry("second");
        second.short_id = "transport2".into();
        second.harness = Some("codex".into());
        // Same first-eight (canonical address) as `first`, different
        // full id -> allowed.
        second.harness_session_id = Some("aaaaaaaa-0000-0000-0000-222222222222".into());
        registry.entries.push(second);
    })
    .unwrap();

    assert_eq!(load_registry(&path).unwrap().entries.len(), 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn state_update_registry_refuses_second_row_claiming_same_full_session_id() {
    let dir = tmpdir("identity-duplicate-session");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut first = sample_entry("first");
        first.harness = Some("codex".into());
        first.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(first);
    })
    .unwrap();

    let result = update_registry(&path, |registry| {
        let mut second = sample_entry("second");
        second.harness = Some("codex".into());
        // SAME full session id under a different row is a tier-0
        // collision: one session, two rows.
        second.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(second);
    });

    assert!(matches!(result, Err(StateError::InvariantViolation(_))));
    assert_eq!(load_registry(&path).unwrap().entries.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn session_transition_classifies_dead_live_and_unknown_predecessors() {
    assert_eq!(
        classify_session_transition("session-a", "session-b", Some(false)),
        SessionTransition::Succession
    );
    assert_eq!(
        classify_session_transition("session-a", "session-b", Some(true)),
        SessionTransition::Branch
    );
    assert_eq!(
        classify_session_transition("session-a", "session-b", None),
        SessionTransition::Deferred
    );
}

#[test]
fn succession_preserves_thread_id_and_branch_keeps_two_rows() {
    let mut predecessor = sample_entry("worker");
    predecessor.fno_id = Some("thread-a".into());
    predecessor.harness_session_id = Some("session-a".into());
    predecessor.short_id = "transport-a".into();
    predecessor.session_id = Some("session-a".into());
    predecessor.codex_session_id = Some("session-a".into());
    predecessor.log_path = Some("/tmp/session-a.log".into());
    predecessor.crown_level = Some(2);
    predecessor.crown_scope = Some("scope-a".into());
    predecessor.crown_grantor = Some("human".into());
    predecessor.mux = Some(MuxRef {
        session: "main".into(),
        pane_id: 4,
    });

    assert!(predecessor.apply_succession("session-a", "session-b"));
    assert_eq!(predecessor.fno_id.as_deref(), Some("thread-a"));
    assert_eq!(predecessor.harness_session_id.as_deref(), Some("session-b"));
    assert_eq!(predecessor.predecessor_session_ids, vec!["session-a"]);
    assert!(!predecessor.apply_succession("session-a", "session-c"));

    let branch =
        predecessor.fork_for_session("worker-branch", "session-c", "session-b", "thread-c");
    assert_eq!(predecessor.harness_session_id.as_deref(), Some("session-b"));
    assert_eq!(branch.harness_session_id.as_deref(), Some("session-c"));
    assert_eq!(branch.forked_from_session_id.as_deref(), Some("session-b"));
    assert_eq!(branch.fno_id.as_deref(), Some("thread-c"));
    assert_ne!(predecessor.fno_id, branch.fno_id);
    assert!(branch.crown_level.is_none());
    assert!(branch.crown_scope.is_none());
    assert!(branch.crown_grantor.is_none());
    assert!(branch.short_id.is_empty());
    assert!(branch.session_id.is_none());
    assert!(branch.codex_session_id.is_none());
    assert!(branch.log_path.is_none());
    assert!(branch.mux.is_none());
    assert!(branch.pid.is_none());
}

#[test]
fn session_lineage_predecessor_full_id_joins_the_successor_row() {
    // x-dfe7 AC6-HP (delivery join): mail/inject naming a succeeded
    // session's full uuid finds the row that now answers as B.
    let mut registry = Registry::default();
    let mut entry = sample_entry("worker");
    entry.harness_session_id = Some("session-b".into());
    entry.related_session_id = None;
    entry.predecessor_session_ids = vec!["session-a".into()];
    registry.entries.push(entry);

    assert!(
        registry.find_name_or_full_session_id("session-a").is_some(),
        "a predecessor full id follows the successor row"
    );
    assert!(
        registry.find_name_or_full_session_id("session-b").is_some(),
        "the current session still joins"
    );
}

// rename_agent: label mutation with identity lock + alias carry.

#[test]
fn rename_agent_renames_and_carries_the_old_label_as_alias() {
    let dir = tmpdir("rename-happy");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut e = sample_entry("worker-a");
        e.harness = Some("claude".into());
        e.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(e);
    })
    .unwrap();

    let (old, new) = rename_agent(&path, "worker-a", "worker-b").unwrap();
    assert_eq!(old, "worker-a");
    assert_eq!(new, "worker-b");

    let reg = load_registry(&path).unwrap();
    let row = reg
        .find("worker-b")
        .expect("renamed row under the new label");
    assert_eq!(row.aliases, vec!["worker-a".to_string()]);
    assert_eq!(
        row.harness_session_id.as_deref(),
        Some("aaaaaaaa-0000-0000-0000-111111111111")
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rename_agent_resolves_by_short_id_and_full_session_id() {
    let dir = tmpdir("rename-tokens");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut e = sample_entry("worker-a");
        e.short_id = "abcd1234".into();
        e.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(e);
    })
    .unwrap();
    rename_agent(&path, "abcd1234", "worker-b").unwrap();
    assert!(load_registry(&path).unwrap().find("worker-b").is_some());

    rename_agent(&path, "aaaaaaaa-0000-0000-0000-111111111111", "worker-c").unwrap();
    let reg = load_registry(&path).unwrap();
    let row = reg.find("worker-c").unwrap();
    assert_eq!(
        row.aliases,
        vec!["worker-a".to_string(), "worker-b".to_string()]
    );

    // Code review: the sibling-verb tiers ride along - a canonical handle
    // (first 8 of the session id) and a related session id resolve too.
    let mut forked = sample_entry("worker-f");
    forked.harness_session_id = Some("bbbbbbbb-0000-0000-0000-222222222222".into());
    forked.related_session_id = Some("cccccccc-0000-0000-0000-333333333333".into());
    update_registry(&path, |registry| registry.entries.push(forked)).unwrap();
    rename_agent(&path, "bbbbbbbb", "worker-g").unwrap();
    assert!(load_registry(&path).unwrap().find("worker-g").is_some());
    rename_agent(&path, "cccccccc-0000-0000-0000-333333333333", "worker-h").unwrap();
    assert!(load_registry(&path).unwrap().find("worker-h").is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rename_agent_refuses_duplicate_and_unknown_and_grammar() {
    let dir = tmpdir("rename-refusals");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut a = sample_entry("worker-a");
        a.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(a);
        registry.entries.push(sample_entry("worker-b"));
    })
    .unwrap();

    let dup = rename_agent(&path, "worker-a", "worker-b").unwrap_err();
    assert!(dup.contains("already names another worker"), "{dup}");
    // Renaming onto a label another row ANSWERS to (its alias) refuses the
    // same way: that label must not be made ambiguous at resolve time.
    rename_agent(&path, "worker-b", "worker-x").unwrap();
    let alias_dup = rename_agent(&path, "worker-b", "worker-a").unwrap_err();
    assert!(
        alias_dup.contains("already names another worker"),
        "{alias_dup}"
    );
    let unknown = rename_agent(&path, "no-such-row", "worker-c").unwrap_err();
    assert!(unknown.contains("no such agent"), "{unknown}");
    let grammar = rename_agent(&path, "worker-a", "bad label!").unwrap_err();
    assert!(grammar.contains("1-64 letters"), "{grammar}");
    // Nothing was written by any refused call (the b->x rename above DID
    // land: worker-x holds it, and "worker-b" answers only as that row's
    // old-label alias, never as a name of its own).
    let reg = load_registry(&path).unwrap();
    assert!(reg.find("worker-a").is_some() && reg.find("worker-x").is_some());
    assert_eq!(
        reg.find("worker-b").map(|e| e.name.as_str()),
        Some("worker-x")
    );
    assert!(reg.find("worker-c").is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rename_agent_by_old_label_after_rename_still_lands_on_the_row() {
    // The "changed before rename" guard needs identity present but the
    // resolved name moved; the alias tier is its observable twin: an old
    // label resolves to the SAME identity, so the write must land.
    let dir = tmpdir("rename-alias-tier");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut e = sample_entry("worker-a");
        e.harness_session_id = Some("aaaaaaaa-0000-0000-0000-111111111111".into());
        registry.entries.push(e);
    })
    .unwrap();
    rename_agent(&path, "worker-a", "worker-b").unwrap();
    rename_agent(&path, "worker-a", "worker-c").unwrap();
    let reg = load_registry(&path).unwrap();
    let row = reg
        .find("worker-c")
        .expect("alias token reached the same row");
    assert_eq!(
        row.aliases,
        vec!["worker-a".to_string(), "worker-b".to_string()]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rename_agent_same_label_is_a_noop() {
    let dir = tmpdir("rename-noop");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        registry.entries.push(sample_entry("worker-a"));
    })
    .unwrap();
    let (old, new) = rename_agent(&path, "worker-a", "worker-a").unwrap();
    assert_eq!((old, new), ("worker-a".to_string(), "worker-a".to_string()));
    let reg = load_registry(&path).unwrap();
    let row = reg.find("worker-a").unwrap();
    assert!(row.aliases.is_empty(), "no self-alias on a no-op rename");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rename_agent_id_less_row_leaves_no_false_removal_accounting() {
    // A LEGACY id-less row (no session id, no short id; on disk from before
    // the resolvable-handle invariant) renamed matches NO removal signal
    // except its alias, and account_for_removed_rows must read the alias or
    // it fires registry_row_removed for a live row. The fixture is
    // hand-written JSON because the write choke point refuses to MINT such
    // a row.
    let dir = tmpdir("rename-id-less");
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "schema_version": REGISTRY_SCHEMA_VERSION,
            "agents": [{
                "name": "worker-a",
                "cwd": "/tmp/x",
                "project_root": "/tmp/x",
                "status": "live",
                "created_at": "2026-05-24T00:00:00Z"
            }]
        })
        .to_string(),
    )
    .unwrap();
    assert!(load_registry(&path).unwrap().find("worker-a").is_some());
    // The choke point itself refuses: a renamed row reads as NEW to
    // validate_resolvable_handle (its old name left the before map), and an
    // id-less row has no handle - so this verb cannot mint the unresolvable
    // row the accounting fear began with. Fail-closed refusal, row intact.
    let refused = rename_agent(&path, "worker-a", "worker-b").unwrap_err();
    assert!(refused.contains("no resolvable handle"), "{refused}");
    assert!(load_registry(&path).unwrap().find("worker-a").is_some());
    assert!(load_registry(&path).unwrap().find("worker-b").is_none());
    // The ALIAS half of the accounting fix still matters for the path the
    // refusal does not reach: a row renamed by Python's retask (which
    // carries aliases) survives a later Rust write without a false
    // registry_row_removed. Covered by the alias membership in
    // account_for_removed_rows' after_names.
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn state_update_registry_allows_retired_suffix_collision() {
    let dir = tmpdir("legacy-suffix-compatible");
    let path = dir.join("registry.json");
    update_registry(&path, |registry| {
        let mut first = sample_entry("first");
        first.short_id = "transport1".into();
        first.harness = Some("codex".into());
        first.harness_session_id = Some("019fb417-0000-0000-0000-111122223333".into());
        registry.entries.push(first);
    })
    .unwrap();
    update_registry(&path, |registry| {
        let mut second = sample_entry("second");
        second.short_id = "transport2".into();
        second.harness = Some("codex".into());
        // Different first-eight (canonical) but same last-eight (retired
        // read-only tier) -> allowed: a retired-tier collision never refuses.
        second.harness_session_id = Some("019fb418-0000-0000-0000-111122223333".into());
        registry.entries.push(second);
    })
    .unwrap();

    assert_eq!(load_registry(&path).unwrap().entries.len(), 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_registry_loads_empty() {
    let dir = tmpdir("missing");
    let reg = load_registry(&dir.join("registry.json")).unwrap();
    assert_eq!(reg.schema_version, REGISTRY_SCHEMA_VERSION);
    assert!(reg.entries.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn python_written_registry_loads_via_typed_path() {
    // Regression for ab-e5a57efa: the typed daemon read path
    // (`load_registry`, used by list/stop/rm/reconcile/status) must parse a
    // registry authored by Python's `registry.write_registry`. That writer
    // uses the top-level `"agents"` key and `AgentEntry` rows that omit the
    // Rust-daemon-only `short_id`/`project_root` fields. Before the fix the
    // whole-file parse failed and `unwrap_or_default()` returned 0 agents.
    let dir = tmpdir("python-registry");
    let path = dir.join("registry.json");
    // Byte-for-byte the shape Python emits (no short_id, no project_root,
    // key is "agents").
    let python_json = r#"{
  "schema_version": 3,
  "agents": [
    {
      "name": "worker-claude",
      "provider": "claude",
      "cwd": "/Users/x/proj",
      "log_path": "/Users/x/.fno/agents/worker-claude.log",
      "claude_short_id": "abc123",
      "codex_session_id": null,
      "gemini_session_id": null,
      "created_at": "2026-05-26T00:00:00Z",
      "status": "live",
      "last_message_at": null,
      "mcp_channel_id": null
    }
  ]
}"#;
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, python_json).unwrap();

    let reg = load_registry(&path).unwrap();
    assert_eq!(reg.entries.len(), 1, "Python-written row must be read");
    let e = reg.find("worker-claude").unwrap();
    assert_eq!(e.harness_name(), "claude");
    assert_eq!(e.status, AgentStatus::Live);
    // v9: the legacy claude_short_id backfills into short_id on load.
    assert_eq!(e.short_id, "abc123");
    assert_eq!(e.legacy_claude_short_id, None); // consumed by the backfill
                                                // The other Rust-only field defaults to empty for Python-authored rows.
    assert_eq!(e.project_root, "");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn python_row_roundtrips_to_python_shape_under_agents_key() {
    // Codex P1 (PR #364): after the daemon rewrites a Python-authored
    // registry (e.g. `rm` removing one agent), the surviving rows must stay
    // readable by Python -- which reads ONLY the top-level `agents` key and
    // whose `AgentEntry(**row)` rejects unknown keys. So the serialized form
    // must (a) use `agents`, not `entries`, and (b) omit every Rust-only
    // field that a Python row lacks (short_id/project_root/session_id/
    // messaging_socket_path/cc_session_id/pid/last_reconciled_at).
    // v10 (x-880e): a Python-authored row is harness-shaped -- harness +
    // harness_session_id, no provider or per-provider session keys.
    let python_json = r#"{"schema_version":10,"agents":[
            {"name":"w","harness":"codex","cwd":"/p","log_path":"/l",
             "harness_session_id":"sid","created_at":"2026-05-26T00:00:00Z",
             "status":"live","last_message_at":null,"mcp_channel_id":null}]}"#;
    let reg: Registry = serde_json::from_str(python_json).unwrap();
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();

    assert!(out.get("agents").is_some(), "must serialize under `agents`");
    assert!(out.get("entries").is_none(), "must NOT serialize `entries`");
    let row = &out["agents"][0];
    for rust_only in [
        "short_id",
        "project_root",
        "session_id",
        "messaging_socket_path",
        "cc_session_id",
        "pid",
        "pid_start_time",
        "last_reconciled_at",
    ] {
        assert!(
            row.get(rust_only).is_none(),
            "Python-authored row must omit Rust-only field `{rust_only}`"
        );
    }
    // v10: the removed identity keys never re-serialize (skip_serializing).
    for removed in [
        "provider",
        "codex_session_id",
        "gemini_session_id",
        "claude_session_uuid",
    ] {
        assert!(
            row.get(removed).is_none(),
            "v10 row must omit removed key `{removed}`"
        );
    }
    // The canonical identity fields survive.
    assert_eq!(row["name"], "w");
    assert_eq!(row["harness"], "codex");
    assert_eq!(row["harness_session_id"], "sid");
}

#[test]
fn v15_model_provider_roundtrips_separately_from_harness() {
    let dual_axis = concat!("open", "code");
    let python_json = format!(
        r#"{{"schema_version":15,"agents":[
            {{"name":"w","harness":"{dual_axis}","provider":"{dual_axis}","model":"glm-5.3","effort":"low","cwd":"/p",
             "log_path":"/l","created_at":"2026-08-19T00:00:00Z","status":"live"}}]}}"#
    );
    let reg: Registry = serde_json::from_str(&python_json).unwrap();
    assert_eq!(reg.entries[0].harness.as_deref(), Some(dual_axis));
    assert_eq!(reg.entries[0].provider.as_deref(), Some(dual_axis));
    assert_eq!(reg.entries[0].model.as_deref(), Some("glm-5.3"));
    assert_eq!(reg.entries[0].effort.as_deref(), Some("low"));

    let out = serde_json::to_value(&reg).unwrap();
    assert_eq!(out["agents"][0]["harness"], dual_axis);
    assert_eq!(out["agents"][0]["provider"], dual_axis);
    assert_eq!(out["agents"][0]["model"], "glm-5.3");
    assert_eq!(out["agents"][0]["effort"], "low");
}

#[test]
fn host_mode_cross_language_round_trip_parity() {
    // interactive-drive node (ab-26b5fe82): the host_mode add must round-trip
    // both directions across the Rust<->Python registry boundary.

    // (a) Rust READS a Python-written row that OMITS host_mode -> exec.
    let no_key = r#"{"schema_version":3,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
    let reg: Registry = serde_json::from_str(no_key).unwrap();
    assert_eq!(reg.entries[0].host_mode, None);
    assert_eq!(reg.entries[0].host_mode_or_default(), HOST_MODE_EXEC);
    assert!(!reg.entries[0].is_interactive());

    // (b) Rust READS a row carrying host_mode="interactive" -> interactive.
    let interactive = r#"{"schema_version":3,"agents":[
            {"name":"bot2","provider":"codex","cwd":"/p","log_path":"/l",
             "codex_session_id":"019e7157","created_at":"2026-05-26T00:00:00Z",
             "status":"live","host_mode":"interactive"}]}"#;
    let reg: Registry = serde_json::from_str(interactive).unwrap();
    assert_eq!(reg.entries[0].host_mode_or_default(), HOST_MODE_INTERACTIVE);
    assert!(reg.entries[0].is_interactive());

    // (c) Rust WRITES an exec row (host_mode None) -> key OMITTED, so a
    // Python AgentEntry(**row) does not gain an unexpected key and Python's
    // missing-key coercion maps the absence back to "exec".
    let mut exec_entry = sample_entry("w");
    exec_entry.host_mode = None;
    let mut reg = Registry::default();
    reg.entries.push(exec_entry);
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert!(
        out["agents"][0].get("host_mode").is_none(),
        "exec row must omit host_mode (skip_serializing_if)"
    );

    // (d) Rust WRITES an interactive row -> host_mode present and readable.
    let mut int_entry = sample_entry("bot2");
    int_entry.host_mode = Some(HOST_MODE_INTERACTIVE.to_string());
    let mut reg = Registry::default();
    reg.entries.push(int_entry);
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert_eq!(out["agents"][0]["host_mode"], "interactive");
}

#[test]
fn route_settings_path_survives_a_daemon_read_modify_write() {
    // v12 (x-ae2d): Python stamps the route path at spawn and the Python
    // relaunch paths read it back. The daemon touches the same rows (GC,
    // screen scrape), so if it dropped this field on write-back the guard
    // would read None on every row it had passed through - a guard on one of
    // N paths, which is no guard. This asserts the passthrough holds.
    const PATH: &str = "/home/me/.fno/route-settings/abc123.json";

    // A row Python stamped: build it through the real serializer and inject
    // only the new key, so this stays a round-trip test rather than a
    // hand-written fixture that can drift from the struct.
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("plain"));
    let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

    // (a) An unrouted row OMITS the key, so Python's AgentEntry(**row) gains
    // no unexpected kwarg.
    assert!(wire["agents"][0].get("route_settings_path").is_none());

    // (b) A pre-v12 row (the same shape, key absent) reads as never-routed
    // rather than as corrupt.
    let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(reg.entries[0].route_settings_path, None);

    // (c) The daemon must re-emit a stamped path, not drop it on write-back.
    wire["agents"][0]["route_settings_path"] = serde_json::Value::from(PATH);
    let reg: Registry = serde_json::from_value(wire).unwrap();
    assert_eq!(reg.entries[0].route_settings_path.as_deref(), Some(PATH));
    let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert_eq!(
        back["agents"][0]["route_settings_path"], PATH,
        "the daemon must re-emit a Python-stamped route path, not drop it"
    );
}

#[test]
fn launch_account_and_related_id_survive_a_daemon_read_modify_write() {
    // v20 (x-d285): Python stamps both at the spawn seams and the re-entry
    // resolver reads them. Same passthrough claim as route_settings_path:
    // a daemon write-back that dropped either key would strip the account
    // axis off every row it touched.
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("plain"));
    let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

    // An unstamped row omits both keys, so Python's AgentEntry(**row) is
    // unaffected and the absence reads as unknown, never as "default".
    assert!(wire["agents"][0].get("launch_account").is_none());
    assert!(wire["agents"][0].get("related_session_id").is_none());

    wire["agents"][0]["launch_account"] = serde_json::Value::from("makers");
    wire["agents"][0]["related_session_id"] = serde_json::Value::from("sess-fork");
    let reg: Registry = serde_json::from_value(wire).unwrap();
    assert_eq!(reg.entries[0].launch_account.as_deref(), Some("makers"));
    assert_eq!(
        reg.entries[0].related_session_id.as_deref(),
        Some("sess-fork")
    );
    let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert_eq!(back["agents"][0]["launch_account"], "makers");
    assert_eq!(back["agents"][0]["related_session_id"], "sess-fork");
}

#[test]
fn related_session_id_resolves_the_row_at_full_tier() {
    // x-d285: a fork's full uuid addresses its row through the related
    // slot at the same tier as the primary - "both ids stay valid
    // forever" means addressable, not merely stored. The name and the
    // primary id keep resolving unchanged.
    let mut reg = Registry::default();
    let mut row = sample_entry("forked");
    row.harness_session_id = Some("01a027ad-fe00-7c12-a116-9ee37c6bdfec".into());
    row.related_session_id = Some("02b118cf-aa11-4d55-9bc2-7f33a1e99110".into());
    reg.entries.push(row);

    assert!(reg.find_name_or_full_session_id("forked").is_some());
    assert!(reg
        .find_name_or_full_session_id("01a027ad-fe00-7c12-a116-9ee37c6bdfec")
        .is_some());
    assert!(
        reg.find_name_or_full_session_id("02b118cf-aa11-4d55-9bc2-7f33a1e99110")
            .is_some(),
        "the related id resolves the row"
    );
    assert!(reg.find_name_or_full_session_id("nosuch").is_none());
}

#[test]
fn launch_account_from_env_is_three_valued() {
    // x-d285: the Rust mint's account fact. An explicit seam id wins; an
    // ambient config dir the mint cannot attribute is unknown; neither
    // present proves the default slot. Environment mutation is process-
    // wide, so the test snapshots and restores both keys around its arms,
    // under the shared env-mutation lock (a sibling test mutating the same
    // keys on another thread would interleave mid-arm snapshots).
    let _guard = ENV_LOCK.lock().unwrap();
    fn restore(launch: Option<std::ffi::OsString>, dir: Option<std::ffi::OsString>) {
        match launch {
            Some(v) => std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, v),
            None => std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY),
        }
        match dir {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }
    let saved_launch = std::env::var_os(LAUNCH_ACCOUNT_ENV_KEY);
    let saved_dir = std::env::var_os("CLAUDE_CONFIG_DIR");

    std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, "makers");
    std::env::set_var("CLAUDE_CONFIG_DIR", "/unrelated");
    let a = launch_account_from_env();

    std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY);
    let b = launch_account_from_env();

    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let c = launch_account_from_env();

    restore(saved_launch, saved_dir);

    assert_eq!(a.as_deref(), Some("makers"), "seam id outranks ambient dir");
    assert_eq!(b, None, "an ambient config dir is unknown, never default");
    assert_eq!(
        c.as_deref(),
        Some("default"),
        "neither present proves default"
    );
}

#[test]
fn launch_account_source_rides_a_concrete_account() {
    // x-04ce P2: a source over "default" or an unknown id is the
    // contradiction "config picked the fallback". The carrier counts only
    // when the value read names a real account, and only caller/config
    // speak the vocabulary. Environment mutation is process-wide, so the
    // test snapshots and restores both keys around its arms, under the
    // same env-mutation lock the three-valued test holds.
    let _guard = ENV_LOCK.lock().unwrap();
    fn restore(launch: Option<std::ffi::OsString>, src: Option<std::ffi::OsString>) {
        match launch {
            Some(v) => std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, v),
            None => std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY),
        }
        match src {
            Some(v) => std::env::set_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY, v),
            None => std::env::remove_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY),
        }
    }
    let saved_launch = std::env::var_os(LAUNCH_ACCOUNT_ENV_KEY);
    let saved_src = std::env::var_os(LAUNCH_ACCOUNT_SOURCE_ENV_KEY);

    std::env::remove_var(LAUNCH_ACCOUNT_ENV_KEY);
    std::env::set_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY, "config");
    let no_id = launch_account_source_from_env();

    std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, "default");
    let default_id = launch_account_source_from_env();

    std::env::set_var(LAUNCH_ACCOUNT_ENV_KEY, "makers");
    let picked = launch_account_source_from_env();

    std::env::remove_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY);
    let flag_only = launch_account_source_from_env();

    std::env::set_var(LAUNCH_ACCOUNT_SOURCE_ENV_KEY, "bogus");
    let off_vocabulary = launch_account_source_from_env();

    restore(saved_launch, saved_src);

    assert_eq!(no_id, None, "a carrier without an account proves nothing");
    assert_eq!(default_id, None, "nobody config-picked the fallback slot");
    assert_eq!(picked.as_deref(), Some("config"), "the pick names itself");
    assert_eq!(
        flag_only.as_deref(),
        Some("caller"),
        "an id without a carrier was on the argv"
    );
    assert_eq!(
        off_vocabulary.as_deref(),
        Some("caller"),
        "a word outside the vocabulary reads as the flag"
    );
}

#[test]
fn origin_survives_a_daemon_read_modify_write() {
    // v16 (x-944f): Python stamps this at row birth, and BOTH watchdog
    // lanes read it - reap as a PROTECTOR (an operator session is never
    // reaped) and retire to answer "did footnote make this row?". The
    // daemon touches the same rows, so dropping it on write-back left the
    // protector unreachable and turned every marked worker back into
    // UNKNOWN, which never retires: the lane goes silently unsatisfiable
    // rather than visibly broken. Measured before this mirror existed:
    // 0 of 37 live rows carried an origin, operator rows the SessionStart
    // hook had stamped included. Same passthrough shape as
    // delivery_policy (v14).
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("leader"));
    let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

    // (a) An unmarked row OMITS the key, so a pre-marker row stays unknown
    // rather than gaining a value nothing wrote.
    assert!(wire["agents"][0].get("origin").is_none());

    // (b) A row written before the field existed reads as unknown.
    let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(reg.entries[0].origin, None);

    // (c) The daemon must re-emit every marker Python writes, not drop it.
    for marker in ["spawn", "operator", "adopted"] {
        wire["agents"][0]["origin"] = serde_json::Value::from(marker);
        let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(reg.entries[0].origin.as_deref(), Some(marker));
        let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
        assert_eq!(back["agents"][0]["origin"], marker);
    }
}

#[test]
fn delivery_policy_survives_a_daemon_read_modify_write() {
    // v14 (x-e21e): Python stamps the policy at register time and the
    // Python send path gates on it. The daemon touches the same rows, so if
    // it dropped this field on write-back the gate would silently revert a
    // bus-only recipient to injectable - the defect again, one daemon tick
    // later. Same passthrough assertion shape as route_settings_path (v12).
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("leader"));
    let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

    // (a) An unmarked row OMITS the key (Python's AgentEntry(**row) gains
    // no unexpected kwarg; every worker keeps the default policy).
    assert!(wire["agents"][0].get("delivery_policy").is_none());

    // (b) A pre-v14 row (key absent) reads as no-policy, not corrupt.
    let reg: Registry = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(reg.entries[0].delivery_policy, None);

    // (c) The daemon must re-emit a stamped policy, not drop it.
    wire["agents"][0]["delivery_policy"] = serde_json::Value::from("bus-only");
    let reg: Registry = serde_json::from_value(wire).unwrap();
    assert_eq!(reg.entries[0].delivery_policy.as_deref(), Some("bus-only"));
    let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert_eq!(
        back["agents"][0]["delivery_policy"], "bus-only",
        "the daemon must re-emit a Python-stamped delivery policy, not drop it"
    );
}

#[test]
fn spawn_trigger_survives_a_daemon_read_modify_write() {
    // v16 (x-944f): the same erasure hit `spawn_trigger`, which shipped
    // under x-42c5 as a Python-only field. One struct change fixes both,
    // so both are asserted.
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("triggered"));
    let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

    assert!(wire["agents"][0].get("spawn_trigger").is_none());

    wire["agents"][0]["spawn_trigger"] = serde_json::Value::from("think_spawn:work-start");
    let reg: Registry = serde_json::from_value(wire).unwrap();
    assert_eq!(
        reg.entries[0].spawn_trigger.as_deref(),
        Some("think_spawn:work-start")
    );
    let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert_eq!(
        back["agents"][0]["spawn_trigger"], "think_spawn:work-start",
        "the daemon must re-emit a Python-stamped spawn trigger, not drop it"
    );
}

#[test]
fn spawned_by_edge_survives_a_daemon_read_modify_write() {
    // x-132c: the parent edge shipped as a Python-only declaration and read
    // 0-of-30 on the live fleet. The struct mirror is what keeps a daemon
    // write-back from erasing it again.
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("child"));
    let mut wire: serde_json::Value = serde_json::to_value(&reg).unwrap();

    assert!(wire["agents"][0].get("spawned_by_session").is_none());

    wire["agents"][0]["spawned_by_session"] =
        serde_json::Value::from("7420e8f7-eeba-4309-8c37-9fc56674f112");
    wire["agents"][0]["spawned_by_harness"] = serde_json::Value::from("claude");
    wire["agents"][0]["spawned_by_cwd"] = serde_json::Value::from("/w");
    let reg: Registry = serde_json::from_value(wire).unwrap();
    assert_eq!(
        reg.entries[0].spawned_by_session.as_deref(),
        Some("7420e8f7-eeba-4309-8c37-9fc56674f112")
    );
    assert_eq!(reg.entries[0].spawned_by_harness.as_deref(), Some("claude"));
    assert_eq!(reg.entries[0].spawned_by_cwd.as_deref(), Some("/w"));
    let back: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert_eq!(
        back["agents"][0]["spawned_by_session"], "7420e8f7-eeba-4309-8c37-9fc56674f112",
        "the daemon must re-emit a Python-stamped parent edge, not drop it"
    );
}

#[test]
fn screen_state_cross_language_round_trip_parity() {
    // v7: the additive `screen_state` verdict must round-trip both
    // directions across the Rust<->Python registry boundary, exactly like
    // inside_leg (v5) and mux (v6) before it.

    // (a) Rust READS a row that OMITS screen_state -> None, no migration.
    let no_key = r#"{"schema_version":6,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
    let reg: Registry = serde_json::from_str(no_key).unwrap();
    assert_eq!(reg.entries[0].screen_state, None);

    // (b) Rust READS a full verdict -> Some, all fields land.
    let with_verdict = r#"{"schema_version":7,"agents":[
            {"name":"pane","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live",
             "screen_state":{"state":"idle","rule":"idle_prompt","seq":3,
                             "at":"2026-07-02T00:00:00Z","ttl_ms":30000}}]}"#;
    let reg: Registry = serde_json::from_str(with_verdict).unwrap();
    let v = reg.entries[0].screen_state.as_ref().unwrap();
    assert_eq!(v.state, "idle");
    assert_eq!(v.rule, "idle_prompt");
    assert_eq!(v.seq, 3);
    assert_eq!(v.at, "2026-07-02T00:00:00Z");
    assert_eq!(v.ttl_ms, Some(30000));

    // (c) Rust WRITES a row without a verdict -> key OMITTED, so a Python
    // AgentEntry(**row) gains no unexpected key.
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("w"));
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert!(
        out["agents"][0].get("screen_state").is_none(),
        "row without a verdict must omit screen_state (skip_serializing_if)"
    );

    // (d) Full round-trip preserves the verdict unchanged.
    let mut scraped = sample_entry("pane");
    scraped.screen_state = Some(ScreenStateReport {
        state: "blocked".into(),
        rule: "permission_prompt".into(),
        seq: 9,
        at: "2026-07-02T01:00:00Z".into(),
        ttl_ms: None,
        answerable: None,
    });
    let mut reg = Registry::default();
    reg.entries.push(scraped.clone());
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert!(
        out["agents"][0]["screen_state"].get("ttl_ms").is_none(),
        "absent ttl_ms omitted"
    );
    let reg2: Registry = serde_json::from_value(out).unwrap();
    assert_eq!(reg2.entries[0].screen_state, scraped.screen_state);
}

#[test]
fn screen_state_report_ttl_ages_and_fails_closed() {
    let now = rfc3339_like_to_secs("2026-07-02T00:01:00Z").unwrap();
    let mk = |at: &str, ttl_ms: Option<u64>| ScreenStateReport {
        state: "working".into(),
        rule: "busy".into(),
        seq: 1,
        at: at.into(),
        ttl_ms,
        answerable: None,
    };
    // No TTL never self-ages; in-TTL live; lapsed expires; corrupt stamp
    // fails closed (a bad `at` must not pin a forever-working badge).
    assert!(mk("2026-07-02T00:00:00Z", None).is_live_at(now));
    assert!(mk("2026-07-02T00:00:30Z", Some(60_000)).is_live_at(now));
    assert!(!mk("2026-07-02T00:00:00Z", Some(5_000)).is_live_at(now));
    assert!(!mk("garbage", Some(60_000)).is_live_at(now));
}

#[test]
fn inside_leg_cross_language_round_trip_parity() {
    // inside-out E3.1 (X2/X3): the additive `inside_leg` field must round-trip
    // both directions across the Rust<->Python registry boundary, like every
    // prior additive RegistryEntry field.

    // (a) Rust READS a Python-written row that OMITS inside_leg -> None.
    let no_key = r#"{"schema_version":5,"agents":[
            {"name":"legacy","provider":"codex","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#;
    let reg: Registry = serde_json::from_str(no_key).unwrap();
    assert_eq!(reg.entries[0].inside_leg, None);

    // (b) Rust READS a full inside-leg report -> Some, lowercase state parses,
    // optional reason/ttl_ms present.
    let with_report = r#"{"schema_version":5,"agents":[
            {"name":"pane","provider":"claude","cwd":"/p","log_path":"/l",
             "created_at":"2026-05-26T00:00:00Z","status":"live",
             "inside_leg":{"state":"working","seq":7,"reason":"running tests",
                           "received_at":"2026-06-27T00:00:00Z","ttl_ms":5000}}]}"#;
    let reg: Registry = serde_json::from_str(with_report).unwrap();
    let rep = reg.entries[0].inside_leg.as_ref().unwrap();
    assert_eq!(rep.state, InsideLegState::Working);
    assert_eq!(rep.seq, 7);
    assert_eq!(rep.reason.as_deref(), Some("running tests"));
    assert_eq!(rep.received_at, "2026-06-27T00:00:00Z");
    assert_eq!(rep.ttl_ms, Some(5000));

    // (c) Rust WRITES a row without a report -> key OMITTED (skip_serializing_if),
    // so a Python AgentEntry(**row) does not gain an unexpected key and a stale
    // reader never sees the field.
    let mut bare = sample_entry("w");
    bare.inside_leg = None;
    let mut reg = Registry::default();
    reg.entries.push(bare);
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    assert!(
        out["agents"][0].get("inside_leg").is_none(),
        "row without a report must omit inside_leg (skip_serializing_if)"
    );

    // (d) Rust WRITES a report -> present, state lowercase, absent reason/ttl
    // omitted (skip_serializing_if on the nested struct).
    let mut withrep = sample_entry("pane");
    withrep.inside_leg = Some(InsideLegReport {
        state: InsideLegState::Done,
        seq: 12,
        reason: None,
        received_at: "2026-06-27T01:00:00Z".into(),
        ttl_ms: None,
    });
    let mut reg = Registry::default();
    reg.entries.push(withrep);
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    let badge = &out["agents"][0]["inside_leg"];
    assert_eq!(badge["state"], "done");
    assert_eq!(badge["seq"], 12);
    assert!(badge.get("reason").is_none(), "absent reason omitted");
    assert!(badge.get("ttl_ms").is_none(), "absent ttl_ms omitted");

    // (e) Full round-trip preserves the report unchanged.
    let reg2: Registry = serde_json::from_value(out).unwrap();
    assert_eq!(
        reg2.entries[0].inside_leg,
        Some(InsideLegReport {
            state: InsideLegState::Done,
            seq: 12,
            reason: None,
            received_at: "2026-06-27T01:00:00Z".into(),
            ttl_ms: None,
        })
    );
}

#[test]
fn rfc3339_like_to_secs_round_trips_known_stamps() {
    // The unix epoch and a couple of fixed dates; values cross-checked against
    // `date -u -d <stamp> +%s`. Proves the days-from-civil inverse matches the
    // daemon's civil() forward direction (the producer of received_at).
    assert_eq!(rfc3339_like_to_secs("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        rfc3339_like_to_secs("2026-06-27T00:00:00Z"),
        Some(1_782_518_400)
    );
    assert_eq!(
        rfc3339_like_to_secs("2026-06-27T00:00:05Z"),
        Some(1_782_518_405)
    );
}

#[test]
fn rfc3339_like_to_secs_rejects_malformed() {
    // Wrong length, bad separators, non-digit, out-of-range fields, and the
    // fractional/offset forms now_rfc3339_like never emits -- all None so the
    // TTL gate fails closed rather than trusting a garbage stamp.
    for bad in [
        "",
        "2026-06-27",
        "2026-06-27T00:00:00",    // no Z
        "2026/06/27T00:00:00Z",   // wrong separators
        "20260627T000000Z",       // compact form, wrong length
        "2026-13-27T00:00:00Z",   // month 13
        "2026-06-27T24:00:00Z",   // hour 24
        "2026-06-27T00:00:00.5Z", // fractional (21 bytes)
        "abcd-ef-ghTij:kl:mnZ",   // non-digit
    ] {
        assert_eq!(rfc3339_like_to_secs(bad), None, "must reject {bad:?}");
    }
}

#[test]
fn inside_leg_is_live_at_ttl_gate() {
    let recv = "2026-06-27T00:00:00Z";
    let recv_secs = rfc3339_like_to_secs(recv).unwrap();
    let rep = |ttl| InsideLegReport {
        state: InsideLegState::Working,
        seq: 1,
        reason: None,
        received_at: recv.into(),
        ttl_ms: ttl,
    };

    // No ttl -> never ages out on its own (cleared by teardown/done/newer report).
    assert!(rep(None).is_live_at(recv_secs + 10_000));

    // ttl=5000ms: live at +4s, live exactly at +5s (<=), expired at +6s (AC-X2-2).
    assert!(rep(Some(5000)).is_live_at(recv_secs + 4));
    assert!(rep(Some(5000)).is_live_at(recv_secs + 5));
    assert!(!rep(Some(5000)).is_live_at(recv_secs + 6));

    // A clock that reads BEFORE received_at (skew) is still live (saturating_sub).
    assert!(rep(Some(5000)).is_live_at(recv_secs.saturating_sub(100)));

    // An unparseable received_at with a ttl fails CLOSED (expired), so a corrupt
    // stamp can never pin a permanent badge.
    let mut corrupt = rep(Some(5000));
    corrupt.received_at = "not-a-stamp".into();
    assert!(!corrupt.is_live_at(recv_secs));
}

#[test]
fn rust_reads_python_row_with_explicit_empty_and_null_fields() {
    // ab-b946b59c: Python's `AgentEntry` now mirrors the Rust-only PTY
    // fields, so its `asdict` emits them for EVERY row -- short_id/
    // project_root as "" (their Rust type is `String`, so a null would fail
    // deserialize) and the Option fields as null. Rust must read that shape.
    let python_json = r#"{"schema_version":4,"agents":[
            {"name":"py-ask","provider":"codex","cwd":"/p","log_path":"/l",
             "short_id":"","project_root":"",
             "claude_short_id":null,"codex_session_id":"sid","gemini_session_id":null,
             "claude_session_uuid":null,"messaging_socket_path":null,"cc_session_id":null,
             "mcp_channel_id":null,"host_mode":"exec",
             "created_at":"2026-05-26T00:00:00Z","status":"exited","last_message_at":null,
             "pid":null,"pid_start_time":null,"last_reconciled_at":null}]}"#;
    let reg: Registry = serde_json::from_str(python_json).unwrap();
    let e = &reg.entries[0];
    assert_eq!(e.name, "py-ask");
    assert_eq!(e.short_id, ""); // "" deserializes into the String field
    assert_eq!(e.project_root, "");
    assert_eq!(e.pid, None); // null -> None for the Option fields
    assert_eq!(e.pid_start_time, None);
    assert_eq!(e.cc_session_id, None);
    assert_eq!(e.codex_session_id.as_deref(), Some("sid"));
    assert!(e.is_one_shot_ask(), "empty short_id + no pid => ask row");
}

#[test]
fn pty_agent_still_serializes_its_short_id() {
    // The skip-when-empty must NOT drop a real daemon agent's short_id/pid.
    let mut reg = Registry::default();
    reg.entries.push(sample_entry("worker-A")); // short_id "worker-A-id", pid Some
    let out: serde_json::Value = serde_json::to_value(&reg).unwrap();
    let row = &out["agents"][0];
    assert_eq!(row["short_id"], "worker-A-id");
    assert_eq!(row["pid"], 1234);
}

#[test]
fn empty_registry_file_loads_default_but_corrupt_file_errors() {
    // Gemini high (PR #364): an empty/whitespace file is a valid empty
    // registry, but a present-but-unparseable file must error LOUDLY rather
    // than default -- otherwise update_registry's read-modify-write republishes
    // the empty default and wipes every other agent.
    let dir = tmpdir("corrupt-registry");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");

    // Empty file -> empty registry, no error.
    std::fs::write(&path, "   \n").unwrap();
    assert!(load_registry(&path).unwrap().entries.is_empty());

    // Corrupt (non-empty, unparseable) file -> error, not silent default.
    std::fs::write(&path, "{ this is not json").unwrap();
    assert!(
        load_registry(&path).is_err(),
        "corrupt registry must surface an error"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_refuses_to_wipe_a_corrupt_registry() {
    // The data-loss path Gemini flagged: update_registry reads, mutates,
    // writes. If the read silently defaulted on a corrupt file, the write
    // would publish an (almost) empty registry. It must instead propagate the
    // parse error and leave the file byte-for-byte intact.
    let dir = tmpdir("no-wipe");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    let corrupt = "{\"schema_version\": 3, \"agents\": [ BROKEN";
    std::fs::write(&path, corrupt).unwrap();

    let result = update_registry(&path, |r| r.entries.push(sample_entry("new-A")));
    assert!(result.is_err(), "update over corrupt registry must error");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        corrupt,
        "corrupt registry must be left untouched, not overwritten"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_upgrades_schema_version_on_write() {
    // Codex P2 (ab-a171ceb2): a Rust write of an existing older store must
    // bump schema_version to the current version, or the forward-compat bump
    // never takes effect for the common case (stores that predate it).
    let dir = tmpdir("upgrade-on-write");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
            &path,
            r#"{"schema_version":3,"agents":[{"name":"w","provider":"codex","cwd":"/p","log_path":"/l","created_at":"2026-05-26T00:00:00Z","status":"live"}]}"#,
        )
        .unwrap();
    update_registry(&path, |r| r.entries.push(sample_entry("w2"))).unwrap();
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        on_disk["schema_version"], REGISTRY_SCHEMA_VERSION,
        "Rust write must upgrade the on-disk schema_version"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A row-shaped raw registry body, the shape the x-a879 loss took from
/// disk: real fields, no test-side struct construction.
fn seeded_registry_body(rows: &[String]) -> String {
    format!(
        r#"{{"schema_version":{},"agents":[{}]}}"#,
        REGISTRY_SCHEMA_VERSION,
        rows.join(",")
    )
}

fn loss_shaped_row(name: &str, harness: &str, sid: Option<&str>) -> String {
    let sid_json = match sid {
        Some(s) => format!(r#""{s}""#),
        None => "null".to_string(),
    };
    format!(
        r#"{{"name":"{name}","short_id":"{name}-id","harness":"{harness}","harness_session_id":{sid_json},"cwd":"/tmp/{name}","created_at":"2026-09-01T10:00:00Z","status":"live"}}"#
    )
}

#[test]
fn update_registry_accounts_for_a_removed_row() {
    // x-a879: a closure that drops a row leaves a receipt and an event,
    // whatever door the closure belongs to.
    let dir = tmpdir("removal-accounting");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[
            loss_shaped_row("kept-a", "claude", Some("a-s")),
            loss_shaped_row("dropped", "claude", Some("dropped-s")),
            loss_shaped_row("kept-b", "codex", Some("b-s")),
        ]),
    )
    .unwrap();

    update_registry(&path, |r| {
        r.entries.retain(|e| e.name != "dropped");
    })
    .unwrap();

    // The write persisted the two survivors.
    let reg = load_registry(&path).unwrap();
    assert_eq!(reg.entries.len(), 2);

    // One per-row event on the agent-lifecycle log, naming the row, the
    // remover, and a staged receipt (the grouped registry_rows_lost line
    // lands after it; the tests for that event assert its own shape).
    let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
    let removals: Vec<serde_json::Value> = events
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["type"] == "registry_row_removed")
        .collect();
    assert_eq!(removals.len(), 1, "exactly one removal event: {events}");
    let event: serde_json::Value = removals[0].clone();
    assert_eq!(event["type"], "registry_row_removed");
    assert_eq!(event["source"], "daemon");
    assert_eq!(event["data"]["name"], "dropped");
    assert_eq!(event["data"]["harness"], "claude");
    assert_eq!(event["data"]["harness_session_id"], "dropped-s");
    assert_eq!(event["data"]["receipt_staged"], true);
    assert_eq!(event["data"]["pid"], std::process::id());
    assert!(
        event["data"]["remover"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the remover is named, not blank: {event}"
    );

    // The receipt sits beside the reap-path ones and says who took the row.
    let receipt_path = home.join("reap-receipts").join("claude-dropped-s.json");
    let receipt: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["row_name"], "dropped");
    assert_eq!(receipt["removed_by"], event["data"]["remover"]);
    assert!(receipt["resume"].as_str().is_some_and(|s| !s.is_empty()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_emits_registry_rows_lost_naming_the_writer() {
    // x-f0d2: beside the per-row receipts, one grouped event names the
    // writer that dropped rows: pid, the verb that ran, and the lost ids
    // with their names. The 09-03 rows vanished with no journal naming a
    // writer; this is that instrument.
    let dir = tmpdir("rows-lost");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[
            loss_shaped_row("kept", "claude", Some("kept-s")),
            loss_shaped_row("dropped", "claude", Some("dropped-s")),
        ]),
    )
    .unwrap();

    update_registry(&path, |r| {
        r.entries.retain(|e| e.name != "dropped");
    })
    .unwrap();

    let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
    let lost: Vec<serde_json::Value> = events
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["type"] == "registry_rows_lost")
        .collect();
    assert_eq!(lost.len(), 1, "exactly one grouped loss event: {events}");
    let data = &lost[0]["data"];
    assert_eq!(data["writer"], "rust");
    assert_eq!(data["pid"], std::process::id());
    assert!(
        data["verb"].as_str().is_some_and(|s| !s.is_empty()),
        "the verb names the door, not just the binary: {}",
        lost[0]
    );
    let ids: Vec<&str> = data["lost"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["harness_session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["dropped-s"]);
    assert_eq!(data["lost"][0]["name"], "dropped");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_emits_nothing_when_nothing_is_removed() {
    let dir = tmpdir("removal-quiet");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[loss_shaped_row("solo", "claude", Some("solo-s"))]),
    )
    .unwrap();

    update_registry(&path, |r| {
        r.entries.push(sample_entry("added"));
    })
    .unwrap();

    assert!(
        !home.join("events.jsonl").exists(),
        "a removal-free write must not open the events stream"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_announces_a_removal_it_cannot_build_a_receipt_for() {
    let dir = tmpdir("removal-no-receipt");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[
            loss_shaped_row("kept", "claude", Some("kept-s")),
            // No session identity: no resume record is renderable.
            loss_shaped_row("identity-less", "claude", None),
        ]),
    )
    .unwrap();

    update_registry(&path, |r| {
        r.entries.retain(|e| e.name != "identity-less");
    })
    .unwrap();

    // The write itself succeeds and the event still announces the removal.
    let reg = load_registry(&path).unwrap();
    assert_eq!(reg.entries.len(), 1);
    let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
    let event: serde_json::Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_eq!(event["data"]["name"], "identity-less");
    assert_eq!(event["data"]["receipt_staged"], false);
    assert!(
        event["data"]["reason"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the announce carries the receipt-build failure: {event}"
    );
    assert!(
        !home.join("reap-receipts").exists(),
        "no receipt file for a row with nothing to resume"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_keeps_a_receipt_the_sweep_already_staged() {
    // The reap sweep writes its receipt BEFORE dropping the rows through
    // this same choke point. The accounting must not rewrite that file
    // with `removed_by`: a pure reap receipt keeps the x-b150 shape.
    let dir = tmpdir("removal-keep-staged");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[loss_shaped_row("swept", "claude", Some("swept-s"))]),
    )
    .unwrap();
    let receipt_path = home.join("reap-receipts").join("claude-swept-s.json");
    std::fs::create_dir_all(receipt_path.parent().unwrap()).unwrap();
    std::fs::write(
        &receipt_path,
        r#"{"row_name":"swept","resume":"claude --resume swept-s"}"#,
    )
    .unwrap();

    update_registry(&path, |r| {
        r.entries.retain(|e| e.name != "swept");
    })
    .unwrap();

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap();
    assert!(
        on_disk.get("removed_by").is_none(),
        "the sweep's receipt was rewritten: {on_disk}"
    );
    let events = std::fs::read_to_string(home.join("events.jsonl")).unwrap();
    let event: serde_json::Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_eq!(event["data"]["receipt_staged"], true);
    assert_eq!(event["data"]["name"], "swept");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_does_not_announce_a_rename() {
    // A rename mutates the name while the session stays: not a removal.
    let dir = tmpdir("removal-rename");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[loss_shaped_row("old-name", "claude", Some("rn-s"))]),
    )
    .unwrap();

    update_registry(&path, |r| {
        if let Some(e) = r.entries.iter_mut().find(|e| e.name == "old-name") {
            e.name = "new-name".into();
        }
    })
    .unwrap();

    assert!(
        !home.join("events.jsonl").exists(),
        "a rename must not read as a removal"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_does_not_announce_a_session_id_backfill() {
    // The late-bind shape: a closure fills harness_session_id on an
    // existing row. The row stays; only its identity token set grows.
    let dir = tmpdir("removal-backfill");
    let home = dir.join("agents");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("registry.json");
    std::fs::write(
        &path,
        seeded_registry_body(&[loss_shaped_row("bound-later", "claude", None)]),
    )
    .unwrap();

    update_registry(&path, |r| {
        if let Some(e) = r.entries.iter_mut().find(|e| e.name == "bound-later") {
            e.harness_session_id = Some("late-bound-s".into());
        }
    })
    .unwrap();

    assert!(
        !home.join("events.jsonl").exists(),
        "a session-id backfill must not read as a removal"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_registry_reads_a_newer_schema_forward() {
    // This assertion was inverted deliberately. The old contract (Codex P2,
    // ab-a171ceb2) refused a future version so a stale reader could not
    // silently drop a field. Refusing turned out to be the worse failure:
    // registry.json is shared by every agent on the machine, so one process
    // ahead of the deployment took the whole fleet's reads down at once.
    // Dropping a field is made safe by refusing to WRITE and by announcing
    // the degrade, not by refusing to look.
    let dir = tmpdir("version-guard");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    let future = REGISTRY_SCHEMA_VERSION + 1;
    std::fs::write(
        &path,
        format!(r#"{{"schema_version":{future},"agents":[]}}"#),
    )
    .unwrap();
    assert!(
        load_registry(&path).is_ok(),
        "a newer writer must not brick this reader"
    );
    std::fs::write(&path, r#"{"schema_version":1,"agents":[]}"#).unwrap();
    assert!(
        load_registry(&path).is_ok(),
        "v1 must still read (back-compat)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_newer_row_with_an_unknown_status_skips_that_row_only() {
    // Added KEYS were only half the problem. AgentStatus has no catch-all
    // variant, so one row carrying a status this binary never heard of used
    // to fail the WHOLE file at serde and take the daemon's reads with it.
    let dir = tmpdir("newer-status");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        // One past THIS binary, derived so a schema bump cannot quietly
        // turn the future-schema fixture into a current-schema one and
        // leave the test asserting a condition it no longer sets up.
        format!(
            r#"{{"schema_version":{},"agents":[
                {{"name":"future","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"hibernating","created_at":"2026-01-01T00:00:00Z"}},
                {{"name":"readable","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"live","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
            REGISTRY_SCHEMA_VERSION + 1
        ),
    )
    .unwrap();

    let reg = load_registry(&path).expect("one unrepresentable row must not brick the read");

    let names: Vec<&str> = reg.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["readable"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_structured_status_from_a_newer_writer_skips_its_row_only() {
    // The three readers must agree about the same file. This row used to be
    // kept as "live" by the raw client path while the typed path skipped it,
    // so `fno agents list` showed a worker the mail path could not see.
    let dir = tmpdir("structured-status");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        // Derived, not literal: see the sibling fixture above.
        format!(
            r#"{{"schema_version":{},"agents":[
                {{"name":"future","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":{{"state":"live","since":1}},"created_at":"2026-01-01T00:00:00Z"}},
                {{"name":"readable","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"live","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
            REGISTRY_SCHEMA_VERSION + 1
        ),
    )
    .unwrap();

    let reg = load_registry(&path).expect("a structured status must not brick the read");

    let names: Vec<&str> = reg.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["readable"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_same_unknown_status_stays_fatal_at_our_own_schema() {
    // At or below our schema an unknown value is a writer bug, not a gap.
    let dir = tmpdir("current-status");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        &format!(
            r#"{{"schema_version":{},"agents":[
                {{"name":"bad","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"hibernating","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
            REGISTRY_SCHEMA_VERSION
        ),
    )
    .unwrap();

    assert!(load_registry(&path).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_refuses_to_write_over_a_newer_schema() {
    // The write block is what makes reading forward safe here.
    let dir = tmpdir("version-write-guard");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    // Derived from the constant so a bump cannot make "newer" mean "ours".
    let newer_version = REGISTRY_SCHEMA_VERSION + 1;
    let newer = format!(r#"{{"schema_version":{newer_version},"agents":[]}}"#);
    std::fs::write(&path, &newer).unwrap();

    match update_registry(&path, |reg| reg.entries.clear()) {
        Err(StateError::UnsupportedSchemaVersion { found, max }) => {
            assert_eq!(found, newer_version);
            assert_eq!(max, REGISTRY_SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        newer,
        "the refused write must leave the newer file byte-identical"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_then_load_roundtrips_and_preserves_optionals() {
    let dir = tmpdir("roundtrip");
    let path = dir.join("registry.json");
    update_registry(&path, |r| r.entries.push(sample_entry("worker-A"))).unwrap();

    // A second update that only flips status must preserve codex_session_id.
    update_registry(&path, |r| {
        r.find_mut("worker-A").unwrap().status = AgentStatus::Idle;
    })
    .unwrap();

    let reg = load_registry(&path).unwrap();
    let e = reg.find("worker-A").unwrap();
    assert_eq!(e.status, AgentStatus::Idle);
    assert_eq!(e.codex_session_id.as_deref(), Some("uuid-1"));
    assert_eq!(e.pid, Some(1234));
    std::fs::remove_dir_all(&dir).ok();
}

/// A row with none of the three legs: no pid identity, no legacy provider
/// (so `backfill_harness_aliases` cannot promote one), no log_path, no
/// harness. Unlike `sample_entry`, which carries a `legacy_provider` that
/// backfill resolves into a real `harness`/`harness_session_id` pair
/// before the write-choke-point guard ever runs.
fn handleless_entry(name: &str) -> RegistryEntry {
    let mut e = sample_entry(name);
    e.legacy_provider = String::new();
    e.codex_session_id = None;
    e.session_id = None;
    e.pid = None;
    e.pid_start_time = None;
    e.log_path = None;
    e.harness = None;
    e.harness_session_id = None;
    e
}

#[test]
fn validate_resolvable_handle_refuses_all_three_legs_empty() {
    let e = handleless_entry("ghost");
    let err = validate_resolvable_handle(&e).expect_err("handle-less row must be refused");
    assert_eq!(
        err,
        "registry row 'ghost' carries no resolvable handle: needs one of \
             (pid + pid_start_time), log_path, or (harness + harness_session_id)"
    );
}

// ── x-665d: refuse a source-ahead bump of the SHARED registry ───────────

#[test]
fn source_root_for_exe_finds_the_checkout_above_a_target_build() {
    let root = tmpdir("src-root-target");
    let exe = root.join("target").join("debug").join("fno-agents");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();

    assert_eq!(source_root_for_exe(&exe, None), Some(root.clone()));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn source_root_for_exe_counts_a_linked_worktrees_git_file() {
    // A linked worktree's `.git` is a FILE. `is_dir()` would miss every
    // worktree, which is the only place a source-ahead build ever runs.
    let root = tmpdir("src-root-worktree");
    let exe = root.join("target").join("debug").join("fno-agents");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(root.join(".git"), b"gitdir: /elsewhere\n").unwrap();

    assert_eq!(source_root_for_exe(&exe, None), Some(root.clone()));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn source_root_for_exe_returns_none_for_a_deployed_binary() {
    let root = tmpdir("src-root-deployed");
    let exe = root.join("cargo").join("bin").join("fno-agents");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();

    assert_eq!(source_root_for_exe(&exe, None), None);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn source_root_for_exe_stops_at_home_so_a_dotfiles_repo_is_not_a_checkout() {
    // `~/.cargo/bin/fno-agents` reaches `$HOME` in three steps. Plenty of
    // people keep a dotfiles repo there; without the stop, every deployed
    // binary on such a machine would read as source-run and refuse writes
    // it must be allowed to make.
    let home = tmpdir("src-root-home");
    let exe = home.join(".cargo").join("bin").join("fno-agents");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::create_dir_all(home.join(".git")).unwrap();

    assert_eq!(source_root_for_exe(&exe, Some(&home)), None);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn source_root_for_exe_stops_at_a_symlinked_home_too() {
    // The caller canonicalizes the exe, so it must canonicalize home as
    // well. A raw `$HOME` that resolves through a symlink never equals the
    // canonical ancestor, the walk runs past home, and a dotfiles repo
    // there turns a deployed binary into a source-run one.
    // Canonicalize the base first: on macOS `/var` is itself a symlink to
    // `/private/var`, so a raw temp path would add a second layer and
    // measure that instead of the one this test is about.
    let real = tmpdir("src-root-symlink-home").canonicalize().unwrap();
    let exe = real.join(".cargo").join("bin").join("fno-agents");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::create_dir_all(real.join(".git")).unwrap();
    let link = real.with_extension("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // The raw (symlinked) spelling does not match the canonical ancestor.
    assert_eq!(source_root_for_exe(&exe, Some(&link)), Some(real.clone()));
    // Canonicalized, as the caller now does, it stops the walk.
    let canonical = link.canonicalize().unwrap();
    assert_eq!(source_root_for_exe(&exe, Some(&canonical)), None);

    std::fs::remove_file(&link).ok();
    std::fs::remove_dir_all(&real).ok();
}

/// The git-managed-$HOME case, constructed rather than observed.
///
/// This developer's machine has an unsymlinked HOME with no `.git` above
/// `~/.fno`, so the inverted guard cannot fire here and a green suite would
/// say nothing about it. A dotfiles repo at `$HOME` is one of the most
/// common setups there is, and footnote ships to those people.
///
/// Writing this test narrowed the claim it was written to defend, which is
/// why it exists. At the DEFAULT registry location the home stop is not what
/// saves a deployed binary: `~/.fno/agents/registry.json` sits inside `$HOME`,
/// so the store-inside-the-root escape hatch already returns "proceed" with
/// or without it. The stop is load-bearing only once the registry lives
/// OUTSIDE home - `FNO_AGENTS_HOME` pointed at `/var/lib/...`, or a
/// relocated `config.paths.agents_registry_path`. Both are asserted below,
/// in both directions, so neither reads as passing by accident.
#[test]
fn a_deployed_binary_under_a_git_managed_home_still_writes() {
    let home = tmpdir("git-managed-home").canonicalize().unwrap();
    std::fs::create_dir_all(home.join(".git")).unwrap();
    let exe = home.join(".cargo").join("bin").join("fno-agents");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    let older = REGISTRY_SCHEMA_VERSION - 1;

    // The default location, inside home. Deployed either way: the
    // store-inside-the-root check answers this one before the stop matters.
    let in_home = home.join(".fno").join("agents").join("registry.json");
    std::fs::create_dir_all(in_home.parent().unwrap()).unwrap();
    assert_eq!(
        source_ahead_root(&exe, Some(&home), &in_home, &in_home, older),
        None
    );
    assert_eq!(
        source_ahead_root(&exe, None, &in_home, &in_home, older),
        None
    );

    // A registry OUTSIDE home (FNO_AGENTS_HOME, or a relocated path). Here
    // the stop is the only thing keeping a deployed binary writable.
    let outside = tmpdir("git-managed-home-store")
        .canonicalize()
        .unwrap()
        .join("registry.json");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    assert_eq!(
        source_ahead_root(&exe, Some(&home), &outside, &outside, older),
        None,
        "a cargo-installed binary must write even when $HOME is a git repo"
    );
    assert_eq!(
        source_ahead_root(&exe, None, &outside, &outside, older),
        Some(home.clone()),
        "without the stop the same deployed binary refuses: this is the bug"
    );

    // SOURCE: a real checkout that is not home still refuses, so the stop
    // did not buy deployment safety by disarming the guard.
    let checkout = home.join("code").join("footnote");
    let src_exe = checkout.join("target").join("debug").join("fno-agents");
    std::fs::create_dir_all(src_exe.parent().unwrap()).unwrap();
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    assert_eq!(
        source_ahead_root(&src_exe, Some(&home), &outside, &outside, older),
        Some(checkout),
        "a source build outside home must still refuse"
    );

    std::fs::remove_dir_all(outside.parent().unwrap()).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_registry_refuses_a_source_ahead_bump_of_the_shared_registry() {
    // AC8-CON: `registry.json` has writers in two languages, so a guard on
    // one leaves the daemon, mux, and every client verb still able to
    // poison the file. This test binary genuinely runs from
    // `<checkout>/target/...`, so it IS the source-run case; pointing
    // FNO_AGENTS_HOME at a tmp dir outside the checkout makes that dir the
    // process-global registry the guard protects.
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = tmpdir("source-ahead-shared");
    let saved = std::env::var_os("FNO_AGENTS_HOME");
    std::env::set_var("FNO_AGENTS_HOME", &home);
    let path = crate::paths::AgentsHome::from_env().registry_json();
    let older = REGISTRY_SCHEMA_VERSION - 1;
    let body = format!(r#"{{"schema_version":{older},"agents":[]}}"#);
    std::fs::write(&path, &body).unwrap();

    let result = update_registry(&path, |reg| reg.entries.clear());

    match saved {
        Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
        None => std::env::remove_var("FNO_AGENTS_HOME"),
    }
    match result {
        Err(StateError::SourceAheadSchemaBump { found, current, .. }) => {
            assert_eq!(found, older);
            assert_eq!(current, REGISTRY_SCHEMA_VERSION);
        }
        other => panic!("expected SourceAheadSchemaBump, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        body,
        "the refused write must leave the file byte-identical"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_registry_bumps_a_named_store_that_is_not_the_shared_one() {
    // AC3-HP, the escape hatch: it works by moving the target, never by
    // silencing the check. `update_registry_upgrades_schema_version_on_write`
    // covers the same ground for the ordinary older-store upgrade; this one
    // states the guard's own condition.
    let dir = tmpdir("named-store-bump");
    let path = dir.join("registry.json");
    let older = REGISTRY_SCHEMA_VERSION - 1;
    std::fs::write(
        &path,
        format!(r#"{{"schema_version":{older},"agents":[]}}"#),
    )
    .unwrap();

    update_registry(&path, |r| r.entries.push(sample_entry("w"))).unwrap();

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(on_disk["schema_version"], REGISTRY_SCHEMA_VERSION);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn validate_resolvable_handle_passes_for_each_leg_alone() {
    let mut leg1 = handleless_entry("pid-only");
    leg1.pid = Some(4242);
    leg1.pid_start_time = Some(9);
    assert!(validate_resolvable_handle(&leg1).is_ok());

    let mut leg2 = handleless_entry("log-only");
    leg2.log_path = Some("/tmp/some.log".into());
    assert!(validate_resolvable_handle(&leg2).is_ok());

    let mut leg3 = handleless_entry("harness-only");
    leg3.harness = Some("claude".into());
    leg3.harness_session_id = Some("sess-1".into());
    assert!(validate_resolvable_handle(&leg3).is_ok());

    // Half of leg1 (pid without pid_start_time) must not pass.
    let mut half_leg1 = handleless_entry("pid-no-start");
    half_leg1.pid = Some(4242);
    assert!(validate_resolvable_handle(&half_leg1).is_err());

    // Half of leg3 (harness without a session id) must not pass.
    let mut half_leg3 = handleless_entry("harness-no-session");
    half_leg3.harness = Some("claude".into());
    assert!(validate_resolvable_handle(&half_leg3).is_err());
}

#[test]
fn update_registry_refuses_a_new_handleless_row() {
    let dir = tmpdir("refuse-handleless");
    let path = dir.join("registry.json");
    // Seed with one legitimate row so the "unchanged on refusal" assertion
    // has real content to check.
    update_registry(&path, |r| r.entries.push(sample_entry("seed"))).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();

    let result = update_registry(&path, |r| r.entries.push(handleless_entry("ghost")));
    match result {
        Err(StateError::InvariantViolation(msg)) => {
            assert!(msg.contains("ghost"), "error must name the row: {msg}");
            assert!(msg.contains("resolvable handle"));
        }
        other => panic!("expected InvariantViolation, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "a refused write must leave the registry unchanged"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_writes_a_new_row_carrying_any_single_leg() {
    let dir = tmpdir("accept-single-leg");
    let path = dir.join("registry.json");

    let mut leg1 = handleless_entry("owns-pid");
    leg1.pid = Some(111);
    leg1.pid_start_time = Some(1);
    update_registry(&path, |r| r.entries.push(leg1)).unwrap();

    let mut leg2 = handleless_entry("owns-log");
    leg2.log_path = Some("/tmp/owns-log.log".into());
    update_registry(&path, |r| r.entries.push(leg2)).unwrap();

    let mut leg3 = handleless_entry("owns-harness");
    leg3.harness = Some("codex".into());
    leg3.harness_session_id = Some("thread-1".into());
    update_registry(&path, |r| r.entries.push(leg3)).unwrap();

    let reg = load_registry(&path).unwrap();
    let names: std::collections::BTreeSet<&str> =
        reg.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        std::collections::BTreeSet::from(["owns-pid", "owns-log", "owns-harness"])
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_registry_never_revalidates_a_preexisting_violating_row() {
    // AC3-FR, the wedge test: a registry that already holds a row
    // violating the invariant (the measured shape -- a legacy row with
    // no pid, no log_path, no harness_session_id) must remain writable
    // forever after. The guard only ever looks at rows absent from the
    // pre-write snapshot.
    let dir = tmpdir("wedge");
    let path = dir.join("registry.json");
    // Written directly (not through update_registry) so no guard has ever
    // seen this row -- exactly how a pre-x-7bcd legacy row landed.
    std::fs::write(
        &path,
        format!(
            r#"{{"schema_version":{REGISTRY_SCHEMA_VERSION},"agents":[
                {{"name":"legacy-ghost","cwd":"/x","log_path":null,
                 "created_at":"2026-01-01T00:00:00Z","status":"live"}}
            ]}}"#
        ),
    )
    .unwrap();

    // An unrelated mutation (touching a DIFFERENT row) must succeed even
    // though "legacy-ghost" still carries no resolvable handle.
    let mut leg2 = handleless_entry("new-and-valid");
    leg2.log_path = Some("/tmp/new-and-valid.log".into());
    let result = update_registry(&path, |r| r.entries.push(leg2));
    assert!(
        result.is_ok(),
        "a pre-existing violation must never wedge an unrelated write: {result:?}"
    );

    let reg = load_registry(&path).unwrap();
    assert!(
        reg.find("legacy-ghost").is_some(),
        "the pre-existing violating row must survive untouched"
    );
    assert!(reg.find("new-and-valid").is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn state_json_absent_is_none_present_roundtrips() {
    let dir = tmpdir("state");
    let path = dir.join("wkA/state.json");
    assert!(load_state(&path).unwrap().is_none());

    let st = AgentState::new_pty("wkA");
    write_state_atomic(&path, &st).unwrap();
    let back = load_state(&path).unwrap().unwrap();
    assert_eq!(back.short_id, "wkA");
    assert_eq!(back.status, AgentStatus::Spawning);
    assert!(back.pty.is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_state_file_treated_as_absent() {
    // Recovery's "registry entry with partial state.json" path: a present
    // but empty file must read as None (-> inconsistent), never an error.
    let dir = tmpdir("empty-state");
    let path = dir.join("state.json");
    std::fs::write(&path, b"").unwrap();
    assert!(load_state(&path).unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn take_active_drive_reads_before_clear() {
    // The recovery ordering invariant in miniature: the returned value
    // carries the session id, and after the call the window is cleared.
    let mut pty = PtyState {
        active: true,
        drive: Some(DriveWindow {
            session_id: Some("drive-uuid".into()),
            mode: Some("interactive".into()),
            last_heartbeat_at_monotonic_ns: Some(42),
        }),
    };
    let taken = pty.take_active_drive().expect("a drive was active");
    assert_eq!(taken.session_id.as_deref(), Some("drive-uuid"));
    assert_eq!(taken.mode.as_deref(), Some("interactive"));
    // Cleared after read.
    assert!(pty.drive.is_none());
    // Idempotent: a second take finds nothing.
    assert!(pty.take_active_drive().is_none());
}

#[test]
fn take_active_drive_none_when_no_drive() {
    let mut pty = PtyState::default();
    assert!(pty.take_active_drive().is_none());
}

#[test]
fn pty_state_wire_shape_is_flat_and_stable() {
    // The Option<DriveWindow> in-memory shape must still serialize to the
    // flat state.json schema (Wave 7 cross-language parity).
    let no_drive = PtyState {
        active: true,
        drive: None,
    };
    assert_eq!(
        serde_json::to_value(&no_drive).unwrap(),
        serde_json::json!({"active": true, "drive_active": false})
    );

    let with_drive = PtyState {
        active: true,
        drive: Some(DriveWindow {
            session_id: Some("d-1".into()),
            mode: Some("interactive".into()),
            last_heartbeat_at_monotonic_ns: Some(99),
        }),
    };
    assert_eq!(
        serde_json::to_value(&with_drive).unwrap(),
        serde_json::json!({
            "active": true,
            "drive_active": true,
            "drive_session_id": "d-1",
            "drive_mode": "interactive",
            "last_heartbeat_at_monotonic_ns": 99
        })
    );
    // Roundtrips back to the same typed value.
    let back: PtyState =
        serde_json::from_value(serde_json::to_value(&with_drive).unwrap()).unwrap();
    assert_eq!(back, with_drive);
}

#[test]
fn pty_state_collapses_inconsistent_legacy_shape() {
    // A legacy/partial file with drive_active:false but a stray session_id
    // deserializes to drive: None - the inconsistent state is normalized
    // away rather than carried.
    let legacy = serde_json::json!({
        "active": true,
        "drive_active": false,
        "drive_session_id": "stray",
    });
    let pty: PtyState = serde_json::from_value(legacy).unwrap();
    assert!(pty.drive.is_none());
}

// (x-4c87 raw-versus-decoded row count family) moved verbatim into its own module: this file is over the
// shrink-only line, and test motion is the sanctioned shrink.
#[path = "x4c87_row_counts.rs"]
mod row_count_tests;
