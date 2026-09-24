//! The blind CPU read's re-read contract, tested against `run_gate` itself.
//! Split from spawn_gate.rs to keep the over-budget file shrinking.

use super::*;

/// The shared admission fixture's payload for one verdict, as the probe
/// would print it: the same file the Python suite pins, so these tests
/// cannot grow their own reading.
fn fixture_payload(verdict: &str) -> String {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../cli/tests/agents/fixtures/spawn_gate_admission.json");
    let doc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&fixture_path)
            .expect("the shared fixture must exist beside the Python suite"),
    )
    .expect("fixture is JSON");
    let case = doc["cases"]
        .as_array()
        .expect("fixture carries cases")
        .iter()
        .find(|c| c["payload"]["admission"]["verdict"] == verdict)
        .unwrap_or_else(|| panic!("fixture carries a {verdict} case"));
    serde_json::to_string(&case["payload"]).unwrap()
}

/// AC1-HP: a waiting spawn re-reads a blind instrument a bounded number
/// of times, then refuses with the sample count on the receipt. The
/// re-read is bounded in TIME too: three reads refuse in seconds, never
/// in QUEUE_TIMEOUT.
#[test]
fn a_blind_read_refuses_after_bounded_rereads() {
    let _g = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-blind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_config = std::env::var_os("FNO_CONFIG");
    std::env::remove_var("FNO_CONFIG");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD");
    let prior_payload_seq = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    let seq = dir.join("payload-seq.txt");
    let blind = format!("{}\n", fixture_payload("undecidable"));
    std::fs::write(&seq, format!("{blind}{blind}{blind}")).unwrap();
    std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", &seq);
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();
    let reg = dir.join("registry.json");

    let started = Instant::now();
    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "w1".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: false,
            },
            ..Default::default()
        },
    );
    let elapsed = started.elapsed();

    match prior_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    match prior_payload_seq {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ"),
    }
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_config {
        Some(value) => std::env::set_var("FNO_CONFIG", value),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    let refusal = got.err().expect("a blind instrument never admits");
    assert_eq!(refusal.exit_code, EXIT_LOAD_REFUSED, "{refusal:?}");
    let receipt = refusal.receipt.expect("refusal carries a receipt");
    assert_eq!(receipt["reason"], "cpu_share_undecidable");
    assert_eq!(receipt["samples"], 3);
    assert!(
        receipt["detail"]
            .as_str()
            .is_some_and(|d| d.contains("cannot be decided")),
        "the receipt carries the reading's own gap sentence: {receipt}"
    );
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC1-ERR (no-wait half): --no-wait keeps one sample, exactly as before
/// the re-read existed.
#[test]
fn no_wait_refuses_a_blind_read_on_the_first_sample() {
    let _g = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-blind-nowait-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_config = std::env::var_os("FNO_CONFIG");
    std::env::remove_var("FNO_CONFIG");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD");
    let prior_payload_seq = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    let seq = dir.join("payload-seq.txt");
    let blind = format!("{}\n", fixture_payload("undecidable"));
    std::fs::write(&seq, format!("{blind}{blind}{blind}")).unwrap();
    std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", &seq);
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();
    let reg = dir.join("registry.json");

    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "w1".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: true,
            },
            ..Default::default()
        },
    );

    match prior_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    match prior_payload_seq {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ"),
    }
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_config {
        Some(value) => std::env::set_var("FNO_CONFIG", value),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    let refusal = got.err().expect("a blind instrument never admits");
    assert_eq!(refusal.exit_code, EXIT_LOAD_REFUSED, "{refusal:?}");
    let receipt = refusal.receipt.expect("refusal carries a receipt");
    assert_eq!(receipt["reason"], "cpu_share_undecidable");
    assert_eq!(receipt["samples"], 1);
    // Positive control: exactly one read consumed one line, two remain.
    assert_eq!(
        std::fs::read_to_string(&seq).unwrap().lines().count(),
        2,
        "one blind read, not three"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC1-EDGE: the point of the re-read. A transient blind read costs one
/// probe, never a worker: the next readable answer admits.
#[test]
fn a_blind_read_then_an_admit_admits() {
    let _g = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-admit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_config = std::env::var_os("FNO_CONFIG");
    std::env::remove_var("FNO_CONFIG");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD");
    let prior_payload_seq = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    let seq = dir.join("payload-seq.txt");
    let admit = r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#;
    std::fs::write(
        &seq,
        format!("ERR footprint unavailable: worker root liveness unavailable\n{admit}\n"),
    )
    .unwrap();
    std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", &seq);
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();
    // An empty registry: the only thing this run must survive past the CPU
    // axis is the slot census, and zero rows against 999 passes it.
    let reg = dir.join("registry.json");
    std::fs::write(
        &reg,
        format!(
            r#"{{"schema_version":{},"entries":[]}}"#,
            crate::state::REGISTRY_SCHEMA_VERSION
        ),
    )
    .unwrap();

    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "w1".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: false,
            },
            ..Default::default()
        },
    );

    match prior_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    match prior_payload_seq {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ"),
    }
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_config {
        Some(value) => std::env::set_var("FNO_CONFIG", value),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    assert!(
        got.is_ok(),
        "a readable admit after a blind read admits: {got:?}"
    );
    // Positive control: both reads happened. The ERR line is consumed, the
    // admit line sticks.
    let left = std::fs::read_to_string(&seq).unwrap();
    assert!(left.contains("\"verdict\":\"admit\""), "{left}");
    assert!(!left.contains("ERR "), "{left}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC1-ERR: an instrument that stays unreadable through the whole budget
/// refuses as cpu_instrument_unreadable, never admits.
#[test]
fn an_unreadable_instrument_never_admits_by_rereading() {
    let _g = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-dead-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_config = std::env::var_os("FNO_CONFIG");
    std::env::remove_var("FNO_CONFIG");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD");
    let prior_payload_seq = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    let seq = dir.join("payload-seq.txt");
    let err_line = "ERR footprint unavailable: worker root liveness unavailable\n".repeat(3);
    std::fs::write(&seq, err_line).unwrap();
    std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", &seq);
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();
    let reg = dir.join("registry.json");

    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "w1".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: false,
            },
            ..Default::default()
        },
    );

    match prior_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    match prior_payload_seq {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ"),
    }
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_config {
        Some(value) => std::env::set_var("FNO_CONFIG", value),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    let refusal = got.err().expect("an unreadable instrument never admits");
    assert_eq!(refusal.exit_code, EXIT_LOAD_REFUSED, "{refusal:?}");
    let receipt = refusal.receipt.expect("refusal carries a receipt");
    assert_eq!(receipt["reason"], "cpu_instrument_unreadable");
    assert_eq!(receipt["samples"], 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A readable CPU admit breaks the run of consecutive blind samples even if
/// the independent slot cap keeps the spawn queued.
#[test]
fn an_admit_resets_blind_samples_before_slot_wait() {
    let _g = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-admit-reset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_config = std::env::var_os("FNO_CONFIG");
    std::env::remove_var("FNO_CONFIG");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD");
    let prior_payload_seq = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    let seq = dir.join("payload-seq.txt");
    let blind = format!("{}\n", fixture_payload("undecidable"));
    let admit = r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#;
    std::fs::write(
        &seq,
        format!("{blind}{admit}\n{blind}{admit}\n{blind}{blind}{blind}"),
    )
    .unwrap();
    std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", &seq);
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 1\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();
    let reg = dir.join("registry.json");
    std::fs::write(
        &reg,
        format!(
            r#"{{"schema_version":{},"entries":[{{"name":"w1","provider":"claude","cwd":"/tmp","status":"live","pid":{},"created_at":"2026-01-01T00:00:00Z"}}]}}"#,
            crate::state::REGISTRY_SCHEMA_VERSION,
            std::process::id()
        ),
    )
    .unwrap();

    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "w2".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: false,
            },
            ..Default::default()
        },
    );

    match prior_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    match prior_payload_seq {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ"),
    }
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_config {
        Some(value) => std::env::set_var("FNO_CONFIG", value),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    let refusal = got.err().expect("three consecutive blind reads refuse");
    assert_eq!(refusal.exit_code, EXIT_LOAD_REFUSED, "{refusal:?}");
    let receipt = refusal.receipt.expect("refusal carries a receipt");
    assert_eq!(receipt["reason"], "cpu_share_undecidable");
    assert_eq!(receipt["samples"], 3);
    let remaining = std::fs::read_to_string(&seq).unwrap();
    assert_eq!(
        remaining.lines().count(),
        1,
        "each ordinary admit resets the consecutive blind-read count; remaining: {remaining:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A blind read breaks the consecutive under-ceiling samples needed to
/// release a spawn that was held on CPU.
#[test]
fn a_blind_sample_breaks_the_held_under_threshold_streak() {
    let _g = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-streak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let original_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let expected_claims_root = dir.join("previous-claims-root");
    std::env::set_var("FNO_CLAIMS_ROOT", &expected_claims_root);
    let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_config = std::env::var_os("FNO_CONFIG");
    std::env::remove_var("FNO_CONFIG");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD");
    let prior_payload_seq = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    let hold = fixture_payload("hold");
    let admit = r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#;
    let blind = fixture_payload("undecidable");
    let seq = dir.join("payload-seq.txt");
    std::fs::write(
        &seq,
        format!("{hold}\n{admit}\n{blind}\n{admit}\n{blind}\n{blind}\n{blind}"),
    )
    .unwrap();
    std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", &seq);
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();
    let reg = dir.join("registry.json");

    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "w1".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: false,
            },
            ..Default::default()
        },
    );

    match prior_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    let observed_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
    match original_claims_root {
        Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
        None => std::env::remove_var("FNO_CLAIMS_ROOT"),
    }
    std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ");
    match prior_payload_seq {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ"),
    }
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_config {
        Some(value) => std::env::set_var("FNO_CONFIG", value),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    let refusal = got
        .err()
        .expect("a blind CPU sample must break the under-threshold streak");
    assert_eq!(refusal.exit_code, EXIT_LOAD_REFUSED, "{refusal:?}");
    let receipt = refusal.receipt.expect("refusal carries a receipt");
    assert_eq!(receipt["reason"], "cpu_share_undecidable");
    assert_eq!(receipt["samples"], 3);
    assert_eq!(
        observed_claims_root,
        Some(expected_claims_root.into_os_string()),
        "the test restores its incoming claims root"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
