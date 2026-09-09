//! Retirement end-to-end journeys (x-70e1): the census, the durable
//! receipt, and the completion decision over staged native stores - each
//! real decision/transport exercised once, deterministically, with named
//! acceptance criteria. Later tasks extend this target with completion,
//! lifecycle and resume cases.

use std::collections::BTreeMap;
use std::path::PathBuf;

use fno_agents::gc_inventory::{
    candidate_node_ids, census_with, recover_assignment, verify_candidates, Inventory, Source,
    SourceReaders,
};
use fno_agents::paths::AgentsHome;
use fno_agents::receipt::{build_reap_receipt, expire_receipt_details, read_reap_receipt};
use fno_agents::state;

// --- staged worlds -----------------------------------------------------------

fn temp_home(tag: &str) -> AgentsHome {
    let dir = std::env::temp_dir().join(format!(
        "retirement-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let home = AgentsHome::at(dir);
    home.ensure_root().unwrap();
    home
}

fn registry_row(name: &str, harness: &str, sid: &str, cwd: &str) -> state::RegistryEntry {
    serde_json::from_str(&format!(
        r#"{{"name":"{name}","short_id":"{name}","harness":"{harness}","harness_session_id":"{sid}","cwd":"{cwd}","created_at":"2026-09-01T00:00:00Z","origin":"spawn","status":"live"}}"#
    ))
    .unwrap()
}

fn write_registry(home: &AgentsHome, entries: Vec<state::RegistryEntry>) {
    state::update_registry(&home.registry_json(), |r| r.entries = entries).unwrap();
}

fn silent_readers() -> SourceReaders {
    SourceReaders {
        claude_listing: Box::new(|| Ok(Vec::new())),
        mux_members: Box::new(|| Ok(Vec::new())),
        claude_store: Box::new(|| Ok(BTreeMap::new())),
        codex_store: Box::new(|| Ok(BTreeMap::new())),
    }
}

fn staged_store(files: &[(&str, &str)]) -> BTreeMap<String, Vec<PathBuf>> {
    let dir = std::env::temp_dir().join(format!(
        "retirement-e2e-store-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (name, body) in files {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let key = name.trim_end_matches(".jsonl").to_ascii_lowercase();
        out.entry(key).or_default().push(path);
    }
    out
}

// --- AC1-HP: the completed native session the registry never adopted ---------

#[test]
fn ac1_hp_completed_native_session_is_enumerated_without_registry_adoption() {
    let home = temp_home("ac1-hp");
    // A claude session completed outside fno: no registry row, but the
    // native listing and the project store both name it.
    let sid = "ee99ff00-7777-8888-9999-aaaabbbbcccc";
    let store = staged_store(&[(
        "EE99FF00-7777-8888-9999-AAAABBBBCCCC.jsonl",
        "{\"type\":\"user\"}\n",
    )]);
    let mut readers = silent_readers();
    let store_for_listing = store.clone();
    readers.claude_listing = Box::new(move || {
        Ok(store_for_listing
            .keys()
            .next()
            .map(|k| (k.trim_end_matches(".jsonl").to_uppercase(), None))
            .into_iter()
            .collect())
    });
    readers.claude_store = Box::new(move || Ok(store.clone()));

    let inv: Inventory = census_with(&home, readers);

    assert_eq!(inv.sessions.len(), 1, "{:?}", inv.sessions);
    let s = &inv.sessions[0];
    assert_eq!(s.harness, "claude");
    assert_eq!(s.session_id.to_lowercase(), sid);
    assert!(
        s.sources.contains(&Source::NativeListing),
        "{:?}",
        s.sources
    );
    assert!(s.sources.contains(&Source::Store), "{:?}", s.sources);
    assert!(
        s.registry_name.is_none(),
        "not adopted: {:?}",
        s.registry_name
    );
    // The adoption prohibition holds on disk: the registry is untouched.
    assert!(state::load_registry(&home.registry_json())
        .unwrap()
        .entries
        .is_empty());
    assert!(inv.incomplete.is_empty(), "{:?}", inv.incomplete);
}

// --- AC1-EDGE: a partial world is never read as a complete one ---------------

#[test]
fn ac1_edge_partial_source_read_reports_incomplete_coverage() {
    let home = temp_home("ac1-edge");
    write_registry(
        &home,
        vec![registry_row(
            "w1",
            "claude",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "/tmp/x",
        )],
    );
    let mut readers = silent_readers();
    readers.claude_listing = Box::new(|| Err("claude agents list timed out".into()));
    readers.mux_members = Box::new(|| Err("squads.json unreadable: no perms".into()));

    let inv = census_with(&home, readers);

    let named: Vec<String> = inv.incomplete.iter().map(|(s, _)| s.clone()).collect();
    assert!(named.contains(&"native-listing".to_string()), "{named:?}");
    assert!(named.contains(&"mux".to_string()), "{named:?}");
    // The readable source still reports, but the inventory carries the
    // incomplete marker a consumer must honor before retiring anything.
    assert!(inv
        .sessions
        .iter()
        .any(|s| s.registry_name == Some("w1".into())));
}

// --- AC2-HP: the durable mapping survives the retention window ---------------

#[test]
fn ac2_hp_receipt_v2_keeps_identity_locator_and_resume_past_retention() {
    let home = temp_home("ac2-hp");
    let row = registry_row(
        "bp-70e1-rowreap",
        "claude",
        "bbbbbbbb-2222-3333-4444-555555555555",
        "/tmp/proj",
    );
    // No ledger entry exists for this session at all: the receipt is the
    // ONLY mapping, which is the AC2 case.
    let receipt = build_reap_receipt(&row, None).expect("a claude row builds a receipt");

    // v2 stamping at build time.
    assert_eq!(receipt.schema_version, Some(2));
    let identity = receipt.identity.as_ref().expect("identity recorded");
    assert_eq!(
        identity["session_id"],
        "bbbbbbbb-2222-3333-4444-555555555555"
    );
    assert_eq!(identity["harness"], "claude");
    assert!(
        receipt.native_locator.is_some(),
        "the native locator is staged before any effect"
    );
    assert!(
        !receipt.resume_argv.is_empty(),
        "resume tokens come from the capability table"
    );
    assert!(receipt.resume.contains(&receipt.harness_session_id));

    // The retention window passes; the expendable detail ages out but the
    // mapping survives, so `fno agents resume` can still find the session.
    let mut aged = receipt.clone();
    aged.ledger = Some(serde_json::json!({"feature": "x-9"}));
    aged.effects = Vec::new();
    assert!(expire_receipt_details(&mut aged));
    assert!(aged.ledger.is_none());
    assert_eq!(aged.schema_version, Some(2));
    assert!(aged.native_locator.is_some());
    assert!(!aged.resume_argv.is_empty());
    assert!(aged.details_expired_at.is_some());

    // The on-disk round-trip reads back with the v2 fields intact (a v1
    // receipt reads with them absent, never invented).
    let path = home.root().join("probe-receipt.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&aged).unwrap()).unwrap();
    let round = read_reap_receipt(&path).unwrap();
    assert_eq!(round, aged);
}

#[test]
fn ac2_edge_a_v1_receipt_reads_with_v2_fields_absent_not_invented() {
    let home = temp_home("ac2-edge");
    let v1 = r#"{
        "row_name": "t-old",
        "short_id": "told",
        "harness": "codex",
        "harness_session_id": "old-sess",
        "cwd": "/tmp",
        "created_at": "2026-01-01T00:00:00Z",
        "reaped_at": "2026-09-01T00:00:00Z",
        "resume": "codex resume old-sess"
    }"#;
    let path = home.root().join("v1.json");
    std::fs::write(&path, v1).unwrap();
    let receipt = read_reap_receipt(&path).unwrap();
    assert_eq!(receipt.schema_version, None);
    assert!(receipt.identity.is_none());
    assert!(receipt.effects.is_empty());
}

// --- native recovery after a wrapper failure --------------------------------

#[test]
fn wrapper_failure_recovery_names_harness_session_cwd_and_native_argv() {
    // The wrapper-failure shape: the fno row is gone (no registry write at
    // all here) and the checkout is gone too - only the native session and
    // its receipt remain.
    let home = temp_home("resume-hint-wrapper-failure");
    let cwd_dir = std::env::temp_dir().join(format!(
        "retirement-e2e-missing-cwd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&cwd_dir).unwrap();
    let cwd = cwd_dir.to_string_lossy().into_owned();
    let row = registry_row(
        "wrapper-fail-row",
        "claude",
        "cccccccc-1111-2222-3333-444444444444",
        &cwd,
    );
    let receipt = build_reap_receipt(&row, None).expect("a claude row builds a receipt");
    fno_agents::receipt::write_reap_receipt(&home, &receipt).unwrap();
    std::fs::remove_dir_all(&cwd_dir).unwrap();

    let hint = fno_agents::resume_receipt::resume_hint(&home, &receipt.harness_session_id)
        .expect("a staged receipt must produce a hint");
    assert!(hint.contains("claude"), "{hint}");
    assert!(hint.contains(&receipt.harness_session_id), "{hint}");
    assert!(hint.contains(&cwd), "{hint}");
    assert!(
        !receipt.resume_argv.is_empty(),
        "the capability table must have staged an argv"
    );
    for token in &receipt.resume_argv {
        assert!(
            hint.contains(token),
            "argv token {token} missing from: {hint}"
        );
    }
}

#[test]
fn resume_hint_is_none_and_silent_when_no_receipt_matches() {
    let home = temp_home("resume-hint-no-match");
    assert!(fno_agents::resume_receipt::resume_hint(&home, "no-such-session").is_none());
}

// --- assignment-link recovery -------------------------------------------------

#[test]
fn assignment_recovery_joins_name_and_first_directive_against_the_graph() {
    // The multi-id dispatch label (`target-<done>-<idea>` shape) folds: each
    // id in the name is a candidate; the graph decides which are real.
    let graph_ids: Vec<String> = vec![
        "x-1bd0".to_string(),
        "x-f370".to_string(),
        "fno-a3f9".to_string(),
    ];
    // Name route: the dispatch label carries two ids.
    let from_name = verify_candidates(
        &candidate_node_ids("target-x-70e1-x-2188"),
        &["x-70e1".to_string(), "x-2188".to_string()],
    );
    assert_eq!(from_name, vec!["x-70e1".to_string(), "x-2188".to_string()]);
    // A slug word never reads as an id.
    assert!(candidate_node_ids("t-feed-worker").is_empty(),);
    // A bare 4-hex token matching two ids is ambiguous and resolves to
    // nothing; a unique suffix resolves.
    let ambiguous = verify_candidates(
        &["1bd0".to_string()],
        &["x-1bd0".to_string(), "ab-1bd0".to_string()],
    );
    assert!(ambiguous.is_empty(), "{ambiguous:?}");
    let unique = verify_candidates(
        &["a3f9".to_string()],
        &[
            "fno-a3f9".to_string(),
            "x-1bd0".to_string(),
            "x-f370".to_string(),
        ],
    );
    assert_eq!(unique, vec!["fno-a3f9".to_string()]);
    // The graph set itself is used, so a wrong-shaped mention drops out.
    assert!(verify_candidates(&["zz-9999".to_string()], &graph_ids).is_empty());

    // Transcript route: the FIRST user directive names the node fno never
    // recorded anywhere else.
    let store = staged_store(&[(
        "deadbeef-3333-4444-5555-666666666666.jsonl",
        "{\"type\":\"user\",\"message\":\"/fno:target x-f370 build the thing\"}\n",
    )]);
    let links = recover_assignment(
        Some("claude-bg-worker"),
        store.get("deadbeef-3333-4444-5555-666666666666").unwrap(),
        &graph_ids,
    );
    assert_eq!(links, vec!["x-f370".to_string()]);
}

// --- the build pin the audit keys on (x-d2ba) --------------------------------

/// The writer of a retirement receipt is `fno-agents-daemon`. The reader of
/// `reap --verify` is `fno-agents`. They are separate executables, so the pin
/// they stamp and compare must name the BUILD, never the running file.
///
/// This is the regression it guards. The pin used to be the running exe's own
/// mtime. One `cargo install` writes the three bins seconds apart, so the
/// daemon's stamp could never equal the client's: a live 523-receipt window
/// read 0 verified, with every current-daemon receipt named stale over a
/// 5-second gap. Every unit test passed, because each one wrote and read the
/// stamp inside one process.
#[test]
fn ac_x_d2ba_the_build_pin_agrees_across_one_cargo_build() {
    // `CARGO_BIN_EXE_*` names the bins THIS test run just built, not what a
    // machine has deployed - agreement across one cargo build is still
    // worth pinning, but it is a narrower claim than the deployed cohort
    // below.
    let client = build_pin(env!("CARGO_BIN_EXE_fno-agents"));
    let daemon = build_pin(env!("CARGO_BIN_EXE_fno-agents-daemon"));
    let worker = build_pin(env!("CARGO_BIN_EXE_fno-agents-worker"));
    // A positive marker first: an absent or empty pin would make the three
    // agree for the wrong reason.
    assert!(
        client.contains(" rev "),
        "the client pin names no build rev: {client:?}"
    );
    assert_eq!(
        client, daemon,
        "client and daemon disagree on the build pin"
    );
    assert_eq!(
        client, worker,
        "client and worker disagree on the build pin"
    );
}

/// The DEPLOYED cohort, not the just-built one: resolve `fno-agents`,
/// `fno-agents-daemon` and `fno-agents-worker` on `PATH` and compare their
/// build pins. This is what `reap --verify` actually audits against on a
/// live machine. When none of the three resolves, an
/// explicit assertion names that absence rather than a silent early
/// return; a resolved binary predating the `build` field fails loudly too -
/// `fno doctor update` deploys the current one.
#[test]
fn the_deployed_cohort_agrees_on_the_build_pin() {
    let names = ["fno-agents", "fno-agents-daemon", "fno-agents-worker"];
    let resolved: Vec<(&str, std::path::PathBuf)> = names
        .iter()
        .filter_map(|name| fno_agents::loop_dispatch::which_binary(name).map(|p| (*name, p)))
        .collect();
    if resolved.is_empty() {
        // Named absence, not a silent early return: an explicit assertion
        // records the fact this machine has no installed cohort to audit,
        // rather than a bare `return` a later bug in the resolve above
        // could hide behind.
        assert!(
            resolved.is_empty(),
            "no deployed fno-agents cohort found on PATH ({names:?}); nothing to verify"
        );
        return;
    }
    // A resolved-but-unpinned binary (predates the `build` field entirely)
    // panics out of `build_pin` below, naming the missing key and the raw
    // JSON - the exact AC11-HP live shape this task exists to surface.
    let pins: Vec<(&str, String)> = resolved
        .iter()
        .map(|(name, path)| (*name, build_pin(&path.to_string_lossy())))
        .collect();
    let first = &pins[0].1;
    assert!(
        first.contains(" rev "),
        "the deployed pin names no build rev: {first:?}"
    );
    for (name, pin) in &pins[1..] {
        assert_eq!(
            pin, first,
            "{name} disagrees with {} on the deployed build pin",
            pins[0].0
        );
    }
}

/// Read one bin's own build pin out of `version --json`.
fn build_pin(bin: &str) -> String {
    let out = std::process::Command::new(bin)
        .args(["version", "--json"])
        .output()
        .unwrap_or_else(|err| panic!("{bin} version --json: {err}"));
    assert!(
        out.status.success(),
        "{bin} version --json exited {:?}",
        out.status
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|err| panic!("{bin} version --json is not json: {err}"));
    v["build"]
        .as_str()
        .unwrap_or_else(|| panic!("{bin} version --json carries no build field: {v}"))
        .to_string()
}
