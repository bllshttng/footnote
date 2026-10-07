//! The slot-census tests: a dead headless holder frees its max_live slot,
//! every counted reservation is named, and the refusal names the release
//! verb. Split from spawn_gate.rs to keep the over-budget file shrinking.

use super::*;

struct SuccessionFixture {
    _agent: crate::spawn_gate_admission::AgentSelfFixture,
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
    registry: std::path::PathBuf,
    pid: u32,
    pid_start: u64,
    prior_home: Option<std::ffi::OsString>,
    prior_claims: Option<std::ffi::OsString>,
    prior_config: Option<std::ffi::OsString>,
    prior_spawn_gate: Option<std::ffi::OsString>,
    prior_payload: Option<std::ffi::OsString>,
    prior_node: Option<std::ffi::OsString>,
}

impl SuccessionFixture {
    fn new(max_live: usize) -> Self {
        let lock = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "fno-gate-succession-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = dir.join("agents-home");
        let claims_root = dir.join("claims-root");
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&claims_root).unwrap();
        std::fs::create_dir_all(&fnodir).unwrap();
        let config = fnodir.join("config.toml");
        std::fs::write(
            &config,
            format!("[agents]\nmax_live = {max_live}\nmin_free_gb = 0\nmax_swap_pct = 0\n"),
        )
        .unwrap();

        let prior_home = std::env::var_os(crate::paths::HOME_ENV);
        let prior_claims = std::env::var_os("FNO_CLAIMS_ROOT");
        let prior_config = std::env::var_os("FNO_CONFIG");
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        let prior_node = std::env::var_os("FNO_NODE");
        let pid = std::process::id();
        let pid_start = crate::daemon::process_start_time(pid).unwrap_or(0);
        let mut fixture = Self {
            _agent: crate::spawn_gate_admission::AgentSelfFixture::set(),
            _lock: lock,
            dir,
            registry: std::path::PathBuf::new(),
            pid,
            pid_start,
            prior_home,
            prior_claims,
            prior_config,
            prior_spawn_gate,
            prior_payload,
            prior_node,
        };
        std::env::set_var(crate::paths::HOME_ENV, &home);
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        std::env::set_var("FNO_CONFIG", &config);
        std::env::remove_var("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_NODE");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );

        fixture.registry = crate::paths::AgentsHome::from_env().registry_json();
        std::fs::create_dir_all(fixture.registry.parent().unwrap()).unwrap();
        fixture.write_entries(Vec::new());
        fixture
    }

    fn row(&self, name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "harness": "claude",
            "cwd": "/tmp",
            "status": "live",
            "created_at": "2026-01-01T00:00:00Z",
            "pid": self.pid,
            "pid_start_time": self.pid_start,
        })
    }

    fn lead(&self, name: &str, session: &str, scope: &str) -> serde_json::Value {
        let mut row = self.row(name);
        row["role_level"] = serde_json::json!(1);
        row["role_scope"] = serde_json::json!(scope);
        row["harness_session_id"] = serde_json::json!(session);
        row
    }

    fn worker(&self, name: &str, session: &str) -> serde_json::Value {
        let mut row = self.row(name);
        row["spawned_by_session"] = serde_json::json!(session);
        row
    }

    fn write_entries(&self, entries: Vec<serde_json::Value>) {
        let registry = serde_json::json!({
            "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
            "entries": entries,
        });
        crate::registry_store::replace_document(&self.registry, registry);
    }

    fn spawn(
        &self,
        name: &str,
        caller_session: Option<&str>,
        succession_scope: Option<&str>,
    ) -> Result<GateGuard, Refusal> {
        run_gate(
            &self.dir,
            &self.registry,
            GateInput {
                name: name.into(),
                substrate: "bg".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                caller_session: caller_session.map(str::to_string),
                succession_scope: succession_scope.map(str::to_string),
                holder_pid: Some(self.pid),
                ..Default::default()
            },
        )
    }
}

impl Drop for SuccessionFixture {
    fn drop(&mut self) {
        fn restore(key: &str, value: Option<std::ffi::OsString>) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        restore(crate::paths::HOME_ENV, self.prior_home.take());
        restore("FNO_CLAIMS_ROOT", self.prior_claims.take());
        restore("FNO_CONFIG", self.prior_config.take());
        restore("FNO_SPAWN_GATE", self.prior_spawn_gate.take());
        restore("FNO_TEST_FOOTPRINT_PAYLOAD", self.prior_payload.take());
        restore("FNO_NODE", self.prior_node.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn plain_spawn_stays_refused_while_valid_succession_reuses_one_slot() {
    let fixture = SuccessionFixture::new(2);
    fixture.write_entries(vec![
        fixture.lead("lead", "session-lead", "x-epic"),
        fixture.worker("worker", "session-lead"),
    ]);

    let refusal = fixture
        .spawn("plain", Some("session-lead"), None)
        .expect_err("ordinary spawn cannot reuse the lead's slot");
    assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
    assert_eq!(refusal.receipt.as_ref().unwrap()["axis"], "max_live");

    fixture
        .spawn("successor", Some("session-lead"), Some("x-epic"))
        .expect("a sole teamed caller replaces its row")
        .release();
}

#[test]
fn succession_skips_a_full_lead_share() {
    let fixture = SuccessionFixture::new(3);
    fixture.write_entries(vec![
        fixture.lead("lead-a", "session-a", "x-epic"),
        fixture.lead("lead-b", "session-b", "y-epic"),
        fixture.worker("worker-a", "session-a"),
    ]);

    fixture
        .spawn("successor", Some("session-a"), Some("x-epic"))
        .expect("a teamed succession does not pay lead share")
        .release();
}

#[test]
fn succession_skips_a_full_lead_share_on_a_multi_epic_scope() {
    let fixture = SuccessionFixture::new(3);
    fixture.write_entries(vec![
        fixture.lead("lead-a", "session-a", "x-epic-a,x-epic-b"),
        fixture.lead("lead-b", "session-b", "y-epic"),
        fixture.worker("worker-a", "session-a"),
    ]);

    fixture
        .spawn("successor", Some("session-a"), Some("x-epic-a,x-epic-b"))
        .expect("a succession over a comma-joined epic set does not pay lead share")
        .release();
}

#[test]
fn succession_refusal_reports_an_ineligible_caller() {
    let fixture = SuccessionFixture::new(2);
    fixture.write_entries(vec![
        fixture.lead("lead-a", "session-a", "x-epic"),
        fixture.lead("lead-b", "session-b", "x-epic"),
    ]);

    let refusal = fixture
        .spawn("successor", Some("session-a"), Some("x-epic"))
        .expect_err("two live holders cannot both succeed the same team");
    assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
    assert_eq!(
        refusal.receipt.as_ref().unwrap()["succession"],
        "caller_not_sole_holder"
    );
}

#[test]
fn succession_refusal_reports_a_scope_held_by_another_caller() {
    let fixture = SuccessionFixture::new(2);
    fixture.write_entries(vec![
        fixture.lead("lead-a", "session-a", "y-epic"),
        fixture.lead("lead-b", "session-b", "x-epic"),
    ]);

    let refusal = fixture
        .spawn("successor", Some("session-a"), Some("x-epic"))
        .expect_err("a different scope holder cannot be vacated by this caller");
    assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
    assert_eq!(
        refusal.receipt.as_ref().unwrap()["succession"],
        "caller_not_sole_holder"
    );
}

#[test]
fn succession_refusal_reports_no_caller_and_a_caller_without_a_live_row() {
    let fixture = SuccessionFixture::new(2);
    fixture.write_entries(vec![
        fixture.lead("lead-a", "session-a", "x-epic"),
        fixture.worker("worker", "session-a"),
    ]);
    let no_caller = fixture
        .spawn("successor-no-caller", None, Some("x-epic"))
        .expect_err("a succession without caller identity cannot replace a row");
    assert_eq!(
        no_caller.receipt.as_ref().unwrap()["succession"],
        "no_caller"
    );

    let mut dead = fixture.row("dead-lead");
    dead["pid"] = serde_json::json!(4_194_321_u32);
    dead["role_level"] = serde_json::json!(1);
    dead["role_scope"] = serde_json::json!("x-epic");
    dead["harness_session_id"] = serde_json::json!("session-dead");
    fixture.write_entries(vec![
        dead,
        fixture.lead("other-lead", "session-other", "y-epic"),
        fixture.worker("other-worker", "session-other"),
    ]);
    let not_live = fixture
        .spawn(
            "successor-dead-caller",
            Some("session-dead"),
            Some("x-epic"),
        )
        .expect_err("a dead caller row cannot release a slot");
    assert_eq!(
        not_live.receipt.as_ref().unwrap()["succession"],
        "caller_not_live"
    );
}

#[test]
fn succession_cannot_take_a_second_slot_at_cap_plus_one() {
    let fixture = SuccessionFixture::new(2);
    let mut successor = fixture.row("pending-successor");
    successor["harness_session_id"] = serde_json::json!("session-successor");
    fixture.write_entries(vec![
        fixture.lead("lead", "session-lead", "x-epic"),
        fixture.worker("worker", "session-lead"),
        successor,
    ]);

    let refusal = fixture
        .spawn("second-successor", Some("session-lead"), Some("x-epic"))
        .expect_err("cap plus one cannot replace another slot");
    assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
    assert_eq!(refusal.receipt.as_ref().unwrap()["count"], 3);
}

#[test]
fn succession_refuses_the_old_lead_after_transfer() {
    let fixture = SuccessionFixture::new(3);
    let mut old_lead = fixture.row("old-lead");
    old_lead["harness_session_id"] = serde_json::json!("session-old");
    fixture.write_entries(vec![
        old_lead,
        fixture.lead("new-lead", "session-new", "x-epic"),
        fixture.worker("worker", "session-new"),
    ]);

    let refusal = fixture
        .spawn("late-successor", Some("session-old"), Some("x-epic"))
        .expect_err("only the holder who will be vacated may succeed");
    assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
    assert_eq!(
        refusal.receipt.as_ref().unwrap()["succession"],
        "caller_not_sole_holder"
    );
}

#[test]
fn succession_refuses_ambiguous_caller_session() {
    let fixture = SuccessionFixture::new(2);
    let rows: Vec<RegistryEntry> = serde_json::from_value(serde_json::json!([
        fixture.lead("lead-a", "session-shared", "x-epic"),
        fixture.lead("lead-b", "session-shared", "x-epic"),
    ]))
    .unwrap();

    assert_eq!(
        spawn_gate_lanes::succession_replaces(&rows, Some("session-shared"), "x-epic", "successor"),
        Err("caller_ambiguous")
    );
}

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
        redeemable: true,
    };
    let live = SlotReservation {
        state: "live",
        pid: Some(std::process::id() as i32),
        redeemable: false,
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

    // All-live reservations name no remedy: releasing a live worker's slot
    // would leave it running uncounted.
    let line = slot_refusal_line(4, 3, 3, &[live], 0, "tail.");
    assert!(!line.contains("claim release"), "{line}");

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
    std::env::set_var("FNO_AGENT_SELF", "gate-fixture-worker");
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

    let claim_path = claims::claim_path("worker:stamp-check", Some(&root)).unwrap();
    let record = crate::claim_store::read_at_path(&claim_path)
        .unwrap()
        .unwrap();
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
    std::env::set_var("FNO_AGENT_SELF", "gate-fixture-worker");
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
    std::env::set_var("FNO_AGENT_SELF", "gate-fixture-worker");
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
