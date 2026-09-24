use super::*;

#[test]
fn retired_require_corroboration_key_still_parses() {
    let _root = crate::paths::DeclaredRoot::declare("retired_require_corroboratio");
    // The key is retired: origin never gates. A config that still
    // carries it must load clean, and the merged settings keep the
    // fields that still gate.
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    std::fs::write(&global, "[review]\nrequired_bots = []\n").unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".fno")).unwrap();
    std::fs::write(
        repo.join(".fno").join("config.toml"),
        "[review]\nrequire_corroboration = true\n",
    )
    .unwrap();
    let inputs = resolve_review_inputs(&repo, None, None, None, Some(&global), None);
    assert_eq!(inputs.settings.required_bots, Some(Vec::<String>::new()));
}

#[test]
fn parse_args_hook_input_stdin_flag() {
    let args: Vec<String> = [
        "loop-check",
        "--state",
        "/s.md",
        "--transcript",
        "/t.jsonl",
        "--cwd",
        "/w",
        "--hook-input-stdin",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let parsed = parse_args(&args).unwrap();
    assert!(parsed.hook_input_stdin);
    // Bare flag must not swallow a following flag as its value.
    assert_eq!(parsed.cwd, PathBuf::from("/w"));
}

/// AC5-ERR: required flags validated in parse_args, which returns Err.
#[test]
fn parse_args_missing_required_flags_err() {
    let no_state: Vec<String> = vec![
        "loop-check".into(),
        "--transcript".into(),
        "/t".into(),
        "--cwd".into(),
        "/c".into(),
    ];
    assert_eq!(
        parse_args(&no_state).unwrap_err(),
        "--state is required".to_string()
    );

    let no_transcript: Vec<String> = vec!["loop-check".into(), "--state".into(), "/s".into()];
    assert_eq!(
        parse_args(&no_transcript).unwrap_err(),
        "--transcript is required".to_string()
    );

    let no_cwd: Vec<String> = vec![
        "loop-check".into(),
        "--state".into(),
        "/s".into(),
        "--transcript".into(),
        "/t".into(),
    ];
    assert_eq!(
        parse_args(&no_cwd).unwrap_err(),
        "--cwd is required".to_string()
    );
}

/// AC5-FR: an unknown flag is tolerated (forward-compat for the shim).
#[test]
fn parse_args_unknown_flag_tolerated() {
    let args: Vec<String> = vec![
        "loop-check".into(),
        "--state".into(),
        "/s".into(),
        "--transcript".into(),
        "/t".into(),
        "--cwd".into(),
        "/c".into(),
        "--future-flag=whatever".into(),
        "--another-unknown".into(),
        "value".into(),
    ];
    let parsed = parse_args(&args).expect("unknown flags must be ignored");
    assert_eq!(parsed.state_path, PathBuf::from("/s"));
    assert_eq!(parsed.transcript_path, PathBuf::from("/t"));
    assert_eq!(parsed.cwd, PathBuf::from("/c"));
}

#[test]
fn budget_flat_key_enforces_cost_cap_ab41b13d9d() {
    // Prove the flat budget_cap key enforces as cost cap for BOTH attended and
    // unattended - this is the fold-in test.
    let settings_cfg = "budget_cap = 0.10\n";
    let settings = parse_settings(settings_cfg);
    assert_eq!(settings.flat_budget_cap, Some(Ok(0.10)));
    // No nested blocks configured
    assert!(settings.attended_cost_cap_usd.is_none());
    assert!(settings.unattended_cost_cap_usd.is_none());
    // The budget resolver picks flat_budget_cap as cost cap fallback
    // for both attended=true and attended=false (tested in check_budget)

    let manifest_att = Manifest {
        session_id: Some("s1".into()),
        created_at: Some("2026-06-05T00:00:00Z".into()),
        attended: true,
        ..Default::default()
    };
    let manifest_unatt = Manifest {
        session_id: Some("s1".into()),
        created_at: Some("2026-06-05T00:00:00Z".into()),
        attended: false,
        ..Default::default()
    };

    // Ledger with cost > 0.10
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("ledger.json");
    std::fs::write(&ledger, r#"[{"session_id":"s1","cost_usd":0.50}]"#).unwrap();

    let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();

    assert_eq!(
        check_budget(&manifest_att, &settings, &now, &ledger),
        Some(BudgetTrip::Cost),
        "flat budget_cap must enforce for attended"
    );
    assert_eq!(
        check_budget(&manifest_unatt, &settings, &now, &ledger),
        Some(BudgetTrip::Cost),
        "flat budget_cap must enforce for unattended"
    );
}

#[test]
fn check_budget_malformed_cost_cap_trips_budget() {
    // Fix 2: malformed cap in manifest -> Budget termination (fail-closed)
    let m = Manifest {
        session_id: Some("s".into()),
        created_at: Some("2026-06-05T00:00:00Z".into()),
        budget_cost_cap_usd: Some(Err("5.OO".into())),
        ..Default::default()
    };
    let s = Settings::default();
    let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("ledger.json");
    std::fs::write(&ledger, r#"[{"session_id":"s","cost_usd":0.0}]"#).unwrap();
    assert_eq!(
        check_budget(&m, &s, &now, &ledger),
        Some(BudgetTrip::Cost),
        "malformed cost cap must fail closed"
    );
}

#[test]
fn check_budget_absent_cap_is_unlimited() {
    // ABSENT caps stay unlimited - must not trip
    let m = Manifest {
        session_id: Some("s".into()),
        created_at: Some("2026-06-05T00:00:00Z".into()),
        ..Default::default()
    };
    let s = Settings::default();
    let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("ledger.json");
    std::fs::write(&ledger, r#"[{"session_id":"s","cost_usd":9999.0}]"#).unwrap();
    assert_eq!(
        check_budget(&m, &s, &now, &ledger),
        None,
        "absent cap must be unlimited"
    );
}

#[test]
fn check_budget_negative_elapsed_no_trip() {
    // Fix 3: created_at in the future (clock skew) -> elapsed=0 -> no wall-clock trip
    let m = Manifest {
        session_id: Some("s".into()),
        // created_at is 1 hour in the future
        created_at: Some("2026-06-05T02:00:00Z".into()),
        budget_wall_clock_cap_minutes: Some(Ok(30)),
        ..Default::default()
    };
    let s = Settings::default();
    // now is earlier than created_at
    let now: DateTime<Utc> = "2026-06-05T01:00:00Z".parse().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("ledger.json");
    std::fs::write(&ledger, "[]").unwrap();
    assert_eq!(
        check_budget(&m, &s, &now, &ledger),
        None,
        "negative elapsed (future created_at) must not trip wall clock cap"
    );
}
