//! The slot-census tests: a dead headless holder frees its max_live slot,
//! every counted reservation is named, and the refusal names the release
//! verb. Split from spawn_gate.rs to keep the over-budget file shrinking.

use super::*;
/// AC2-EDGE: the refusal sentence names the release remedy for a counted
/// reservation, preferring the suspect one a dead holder left behind.
#[test]
fn slot_refusal_line_names_the_release_remedy() {
    let suspect = SlotReservation {
        name: "w-res".into(),
        holder: "spawn-gate:99:w-res".into(),
        pid: Some(99),
        age_s: Some(120),
        state: "suspect",
        provider: Some("__uncapped__".into()),
    };
    let live = SlotReservation {
        state: "live",
        pid: Some(std::process::id() as i32),
        ..suspect.clone()
    };

    let line = slot_refusal_line(4, 3, 3, &[live.clone(), suspect.clone()], 0, "tail.");
    assert!(
        line.contains("fno agents claim release worker:w-res --force"),
        "{line}"
    );
    assert!(
        line.contains("--reason \"<why>\""),
        "the clause keeps the reason template: {line}"
    );

    // All-live reservations still get a name (the first counted).
    let line = slot_refusal_line(4, 3, 3, &[live], 0, "tail.");
    assert!(
        line.contains("fno agents claim release worker:w-res --force"),
        "{line}"
    );

    // Registry-only saturation names no remedy.
    let line = slot_refusal_line(3, 3, 3, &[], 0, "tail.");
    assert!(!line.contains("claim release"), "{line}");
}

/// AC2-EDGE: a saturated cap with one counted reservation names it in the
/// `--no-wait` receipt's `slot_rows`, beside the registry-row names.
#[test]
fn no_wait_refusal_receipt_names_a_reservation() {
    let _g = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-nwres-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
    std::env::remove_var("FNO_SPAWN_GATE");
    let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
    std::env::set_var(
        "FNO_TEST_FOOTPRINT_PAYLOAD",
        r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
    );
    let fnodir = dir.join(".fno");
    std::fs::create_dir_all(&fnodir).unwrap();
    std::fs::write(
        fnodir.join("config.toml"),
        "[agents]\nmax_live = 1\nmin_free_gb = 0\nmax_swap_pct = 0\n",
    )
    .unwrap();

    // The reservation is the only counted slot: cap 1, empty registry.
    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: Some(root.clone()),
    };
    acquire_worker_slot(
        &mut guard,
        "w-res",
        "spawn-gate:me:w-res",
        std::process::id(),
        None,
        false,
    )
    .unwrap();
    let reg = dir.join("registry.json");
    std::fs::write(&reg, r#"{"schema_version":1,"entries":[]}"#).unwrap();

    let got = run_gate(
        &dir,
        &reg,
        GateInput {
            name: "new-spawn".into(),
            substrate: "bg".into(),
            flags: GateFlags {
                force: false,
                no_wait: true,
            },
            ..Default::default()
        },
    );

    std::env::remove_var("FNO_CLAIMS_ROOT");
    match prior_spawn_gate {
        Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
        None => std::env::remove_var("FNO_SPAWN_GATE"),
    }
    match prior_payload {
        Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
        None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
    }

    let refusal = got.err().expect("cap 1 with one reservation must refuse");
    assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
    let receipt = refusal.receipt.expect("no_wait refusal carries a receipt");
    assert_eq!(receipt["count"], 1);
    let names: Vec<String> = receipt["slot_rows"]
        .as_array()
        .expect("slot_rows array")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, ["w-res"], "{names:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC1-EDGE: the claim written by `acquire_worker_slot` records the
/// holder pid it was given (the Python transport's pid, not the verb
/// process's own) and stamps it `holder-process`, so the classifier can
/// free the lease when that pid dies.
#[test]
fn rust_headless_slot_claim_stamps_holder_pid_and_provenance() {
    let root = std::env::temp_dir().join(format!(
        "fno-gate-stamp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: Some(root.clone()),
    };

    let holder_pid: u32 = 4_194_321; // mirrors the fixture's never-live pid
    acquire_worker_slot(
        &mut guard,
        "stamp-check",
        "spawn-gate:test",
        holder_pid,
        None,
        false,
    )
    .unwrap();

    let claim_path = root
        .join(".fno/claims")
        .join(format!("{}.lock", claims::encode_key("worker:stamp-check")));
    let raw = std::fs::read_to_string(claim_path).unwrap();
    let record: claims::ClaimRecord = serde_yaml_ng::from_str(&raw).unwrap();
    assert_eq!(record.pid, Some(holder_pid as i32), "{record:?}");
    assert_eq!(
        record.pid_provenance.as_deref(),
        Some(claims::HOLDER_PROCESS),
        "{record:?}"
    );
    guard.release();
    std::fs::remove_dir_all(root).ok();
}

/// AC1-HP: a headless reservation whose holder pid is dead reads Stale,
/// counts 0, and frees its slot while the TTL is still unexpired.
#[test]
fn dead_holder_frees_its_headless_slot() {
    let _g = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-deadhold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);

    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: Some(root.clone()),
    };
    acquire_worker_slot(
        &mut guard,
        "dead-judge",
        "spawn-gate:99:dead-judge",
        4_194_321,
        None,
        false,
    )
    .unwrap();

    // Assert while the guard holds: release() would delete the claim and
    // read Free, which proves nothing about the classifier.
    let mut warnings = Vec::new();
    let got = live_worker_slot_claims(&mut warnings);
    assert!(got.is_empty(), "dead holder must free the slot: {got:?}");
    let (state, _) = claims::status("worker:dead-judge", Some(&root));
    assert_eq!(state, claims::ClaimState::Stale);

    guard.release();
    std::env::remove_var("FNO_CLAIMS_ROOT");
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC1-ERR: a live holder keeps its slot and the census names it.
#[test]
fn live_holder_keeps_its_headless_slot() {
    let _g = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-livehold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("claims-root");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);

    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: Some(root.clone()),
    };
    acquire_worker_slot(
        &mut guard,
        "w-res",
        "spawn-gate:me:w-res",
        std::process::id(),
        None,
        false,
    )
    .unwrap();

    let mut warnings = Vec::new();
    let got = live_worker_slot_claims(&mut warnings);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].name, "w-res");
    assert_eq!(got[0].state, "live");
    assert_eq!(got[0].pid, Some(std::process::id() as i32));
    assert!(got[0].age_s.is_some());
    assert_eq!(
        got[0].provider.as_deref(),
        Some(KNOWN_UNROUTED_PROVIDER),
        "unrouted spawn stamps the un-routed marker"
    );

    std::env::remove_var("FNO_CLAIMS_ROOT");
    let _ = std::fs::remove_dir_all(&dir);
}

/// AC2-ERR: a corrupted `worker:` claim file is not counted, not named,
/// and still pushes the warning.
#[test]
fn corrupted_slot_claim_is_skipped_and_warned() {
    let _g = claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fno-gate-corrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("claims-root");
    std::fs::create_dir_all(root.join(".fno/claims")).unwrap();
    std::env::set_var("FNO_CLAIMS_ROOT", &root);
    let claim_path = root
        .join(".fno/claims")
        .join(format!("{}.lock", claims::encode_key("worker:broken")));
    std::fs::write(&claim_path, "{ not yaml").unwrap();

    let mut warnings = Vec::new();
    let got = live_worker_slot_claims(&mut warnings);
    assert!(got.is_empty(), "{got:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("corrupted slot claim worker:broken")),
        "{warnings:?}"
    );

    std::env::remove_var("FNO_CLAIMS_ROOT");
    let _ = std::fs::remove_dir_all(&dir);
}
