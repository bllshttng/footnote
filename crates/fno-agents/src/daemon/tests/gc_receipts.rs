//! The gc ladder, reap-receipt gate and plan_reconcile test families,
//! moved verbatim out of daemon.rs (file budget shrink). Shared helpers
//! (`tmp_home`, `ask_row`, `rentry`, `no_tails`, `probe`, `civil`, ...)
//! stay in the parent tests module and resolve through the glob.
use super::*;

// ── x-91f3: is_live from the ladder, and every keep is named ────────────

/// The acceptance, against a fixture registry shaped like the real one:
/// most rows carry neither `short_id` nor `pid` (structural for codex,
/// whose sessions a shared app-server hosts), their stored status is
/// live-ish, and two rows only the ladder can vouch for - an unstamped
/// claude row whose transcript truth state reads `working` (named LIVE),
/// and a stamped codex row whose heartbeat advanced past its own exit
/// stamp (resolved as live: the false stamp is cleared and the clear is
/// reported). Before x-91f3 every row here read not-live and landed in
/// the silent `_ => {}` arm: `fno agents reap --dry-run` named zero of
/// the rows it kept. This test cannot pass with `is_live` wrong: the
/// truth-alive row must be named LIVE, the surface-less rows NOT
/// TERMINAL, the named set must cover every row, and the sweep must
/// still reap NOTHING - the fix is a classification-and-reporting
/// change, never a removal.
#[test]
fn gc_sweep_names_every_kept_row_on_a_real_shaped_registry() {
    let home = tmp_home("gc-names-kept");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    let (y2, mo2, d2, h2, mi2, s2) = civil(now - 3600);
    let beat_at = format!("{y2:04}-{mo2:02}-{d2:02}T{h2:02}:{mi2:02}:{s2:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        // The majority shape on the measured registry: live-ish status,
        // no short_id, no pid, nothing for a process surface to vouch
        // with.
        for name in ["idle-a", "idle-b", "idle-c"] {
            let mut e = ask_row(name, None);
            e.status = AgentStatus::Idle;
            r.entries.push(e);
        }
        // Unstamped, surface-less, alive only through the truth rung: a
        // claude row whose transcript says the session is working. The
        // persisted session id is harness_session_id; the ladder reads
        // the claude uuid backfilled from it on load. The old derivation
        // called this row not-live; the ladder calls it live.
        let mut truth_live = ask_row("truth-live", None);
        truth_live.status = AgentStatus::Idle;
        truth_live.harness = Some("claude".into());
        r.entries.push(truth_live);
        // Stamped, surface-less, alive only through the heartbeat rung:
        // received strictly later than the row's own exit stamp.
        let mut heartbeat_live = codex_thread_row("cx-live", Some(exited_at.as_str()));
        heartbeat_live.status = AgentStatus::Idle;
        heartbeat_live.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: beat_at,
            ttl_ms: None,
        });
        r.entries.push(heartbeat_live);
    })
    .unwrap();

    // The ladder as production folds it: the truth batch over the
    // fixture rows with the tail read staged (one uuid answers
    // `working`), then the production prober over that map - the truth
    // rung reaches the ladder through the SAME seam the daemon tick
    // uses, batch included. The socket rung reads the real sessions
    // index but every fixture row carries an empty short_id, so the
    // rung is skipped for all of them; the only markers that can fire
    // are the truth rung and the heartbeat rung, the markers pid-less
    // rows are judged by.
    let entries = state::load_registry(&home.registry_json()).unwrap().entries;
    let truth = batched_row_states(&entries, &|handles: &[String]| {
        handles
            .iter()
            .filter(|h| h.as_str() == "truth-live-sess")
            .map(|h| (h.clone(), "working".to_string()))
            .collect()
    });
    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_liveness_prober(truth),
        &|_| None,
    );

    assert!(summary.reaped.is_empty(), "{:?}", summary.reaped);
    assert!(summary.reaped_backstop.is_empty());
    assert!(summary.reaped_dormant.is_empty());
    // NON-ZERO, per gate: the truth-alive row reads live through the
    // ladder, the surface-less rows keep as not-terminal - each named.
    assert_eq!(summary.kept_live, vec!["truth-live".to_string()]);
    assert_eq!(
        summary
            .kept_not_terminal
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        vec![
            "idle-a".to_string(),
            "idle-b".to_string(),
            "idle-c".to_string()
        ]
    );
    // The heartbeat row's stale stamp is CLEARED on the ladder's positive
    // answer (the x-5d96 resolution, reported - never a reap): a kept
    // stamp would hand gc an old clock that skips grace at the row's
    // real death.
    assert_eq!(summary.cleared_contradiction, vec!["cx-live".to_string()]);
    // And the named set covers EVERY row: no silent keep survives.
    let named: std::collections::BTreeSet<String> = summary
        .kept_live
        .iter()
        .chain(summary.kept_not_terminal.iter().map(|(id, _)| id))
        .chain(summary.kept_contradicted.iter())
        .chain(summary.cleared_contradiction.iter())
        .chain(summary.kept_uncorroborated.iter())
        .chain(summary.kept_dirty.iter().map(|(id, _)| id))
        .chain(summary.kept_no_receipt.iter().map(|(id, _)| id))
        .cloned()
        .collect();
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let live_row = reg.entries.iter().find(|e| e.name == "cx-live").unwrap();
    assert!(
        live_row.exited_at.is_none(),
        "the false stamp is gone from the proven-alive row"
    );
    assert_eq!(
        named.len(),
        reg.entries.len(),
        "a kept row the report does not name: named={named:?}"
    );
    assert!(
        reg.entries.iter().all(|e| named.contains(e.name.as_str())),
        "every row still on disk, each one named"
    );
}

#[test]
fn the_truth_batch_includes_unstamped_rows() {
    // The ladder's is_live vote reads the truth rung for EVERY row, so a
    // stamped-only batch leaves the transcript - the one positive marker
    // an unstamped, pid-less claude row can carry - permanently silent
    // for that vote.
    let stamped = {
        let mut e = ask_row("stamped", Some("2020-01-01T00:00:00Z"));
        e.claude_session_uuid = Some("stamped-uuid".into());
        e
    };
    let unstamped = {
        let mut e = ask_row("unstamped", None);
        e.claude_session_uuid = Some("unstamped-uuid".into());
        e
    };
    let bare = ask_row("bare", None); // no uuid: must not reach the probe
    let seen: std::cell::RefCell<Vec<Vec<String>>> = std::cell::RefCell::new(Vec::new());
    let capture = |handles: &[String]| {
        seen.borrow_mut().push(handles.to_vec());
        std::collections::HashMap::new()
    };
    batched_row_states(&[stamped, unstamped], &capture);
    assert_eq!(
        seen.borrow().len(),
        1,
        "one batched call, however many rows"
    );
    let mut handles = seen.borrow_mut().pop().unwrap();
    handles.sort();
    assert_eq!(
        handles,
        vec!["stamped-uuid".to_string(), "unstamped-uuid".to_string()]
    );
    seen.borrow_mut().clear();
    batched_row_states(&[bare], &capture);
    assert!(
        seen.borrow().is_empty(),
        "an empty candidate set spends nothing"
    );
}

// ── x-b150: the reap receipt gate ────────────────────────────────────────

/// A past-grace dead row with no ledger entry still reaps, and the receipt
/// on disk is built from the ROW: resume command included, fields the
/// ledger never carried for this row present. This is the 12-of-26
/// population (kings, blueprint and rescue sessions) the ledger reader
/// cannot serve.
#[test]
fn reap_receipt_built_from_the_row_when_the_ledger_has_no_entry() {
    let home = tmp_home("gc-receipt-row");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("king-mux", Some(exited_at.as_str()));
        e.short_id = "kingmux".into();
        e.log_path = Some("/tmp/king-mux.log".into());
        e.pid = Some(999_999_999); // no such process: confirmed dead
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert_eq!(summary.reaped, vec!["kingmux".to_string()]);
    assert!(summary.kept_no_receipt.is_empty());
    // The row is gone AND the record of how to come back is on disk.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().all(|e| e.name != "king-mux"));
    let path = home
        .root()
        .join("reap-receipts")
        .join("claude-king-mux-sess.json");
    let raw = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("receipt must be durable before the row leaves: {e}"));
    let receipt: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(receipt["row_name"], "king-mux");
    assert_eq!(receipt["harness"], "claude");
    assert_eq!(receipt["harness_session_id"], "king-mux-sess");
    assert_eq!(receipt["cwd"], "/tmp");
    assert_eq!(receipt["log_path"], "/tmp/king-mux.log");
    assert_eq!(receipt["created_at"], "2020-01-01T00:00:00Z");
    assert_eq!(receipt["resume"], "claude --resume king-mux-sess");
    assert!(
        receipt.get("ledger").is_none(),
        "no ledger entry exists for this session; none may be invented"
    );
}

/// A row the policy would remove but whose receipt cannot be built is
/// Unknown, and unknown never reaps: no registry write, a named gate in
/// the report, no receipt file.
#[test]
fn a_row_whose_receipt_cannot_be_built_is_never_reaped() {
    let home = tmp_home("gc-receipt-unknown");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("no-cap-row", Some(exited_at.as_str()));
        e.short_id = "nocap".into();
        // A harness with no capability row at all (hermes hosts real
        // sessions per docs/SETUP-*.md and ships no row) carries a
        // session identity but no declared resume form: the Unknown
        // case, by name. grok carried this fixture until x-fd31 landed
        // its row; hermes has no row to land.
        e.harness = Some("hermes".into());
        e.pid = Some(999_999_999); // confirmed dead: nothing else holds it
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert!(summary.reaped.is_empty());
    assert_eq!(summary.kept_no_receipt.len(), 1);
    let (id, reason) = &summary.kept_no_receipt[0];
    assert_eq!(id, "nocap");
    assert!(reason.contains("hermes"), "{reason}");
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.name == "no-cap-row"));
    assert!(!home.root().join("reap-receipts").exists());
}

/// x-6db9: the retention sweep. A receipt past the window expires in the
/// same GC sweep that writes new ones; the fresh receipt a sweep just
/// wrote carries `reaped_at` of now and is never its own expiry's victim.
fn write_receipt_at(home: &AgentsHome, name: &str, reaped_at: &str) -> std::path::PathBuf {
    let dir = home.root().join("reap-receipts");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let receipt = serde_json::json!({
        "row_name": "t-expiry",
        "short_id": "texpiry",
        "harness": "claude",
        "harness_session_id": "expiry-sess",
        "cwd": "/tmp",
        "created_at": "2020-01-01T00:00:00Z",
        "reaped_at": reaped_at,
        "resume": "claude --resume expiry-sess",
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    path
}

fn rfc3339_days_ago(days: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - days * 86_400);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[test]
fn a_receipt_past_the_window_expires_in_the_same_sweep() {
    let home = tmp_home("gc-receipt-expired");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let old = write_receipt_at(&home, "claude-old-sess.json", &rfc3339_days_ago(8));

    let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600), 7);

    assert!(!old.exists(), "the receipt past the window must be gone");
    assert_eq!(
        summary.expired_receipts,
        vec!["claude-old-sess.json".to_string()]
    );
}

#[test]
fn a_receipt_inside_the_window_survives_and_the_window_flows() {
    let home = tmp_home("gc-receipt-kept");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let recent = write_receipt_at(&home, "claude-recent-sess.json", &rfc3339_days_ago(6));

    let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600), 7);
    assert!(recent.exists());
    assert!(summary.expired_receipts.is_empty());

    // The same 6-day-old receipt under a 5-day window expires: the
    // configured value reaches the sweep, the default is not hardcoded.
    let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600), 5);
    assert!(!recent.exists());
    assert_eq!(
        summary.expired_receipts,
        vec!["claude-recent-sess.json".to_string()]
    );
}

#[test]
fn a_receipt_whose_reaped_at_will_not_parse_is_kept_and_named() {
    let home = tmp_home("gc-receipt-baddate");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let broken = write_receipt_at(&home, "claude-broken-sess.json", "");
    let missing = write_receipt_at(&home, "claude-missing-sess.json", "not-a-date");

    let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(3600), 7);

    // A failed read is not evidence of age: both survive, and the sweep
    // names what it kept so a silent pile-up is never mistaken for health.
    assert!(broken.exists());
    assert!(missing.exists());
    assert!(summary.expired_receipts.is_empty());
    let kept: Vec<&str> = summary
        .kept_receipts
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(kept.contains(&"claude-broken-sess.json"), "{kept:?}");
    assert!(kept.contains(&"claude-missing-sess.json"), "{kept:?}");
}

/// Dry-run classifies but never writes - and never deletes: a rehearsal
/// that pruned real receipts would not be a rehearsal.
#[test]
fn dry_run_never_expires_receipts() {
    let home = tmp_home("gc-receipt-dryrun");
    let old = write_receipt_at(&home, "claude-old-sess.json", &rfc3339_days_ago(30));

    let summary = gc_sweep_dry_run(&home, &|_| Duration::from_secs(3600));

    assert!(old.exists());
    assert!(summary.expired_receipts.is_empty());
}

/// The receipt's resume command is rendered from the capability table,
/// never a local literal: the test renders the declared form itself and
/// compares, so a harness whose form changes cannot silently strand a
/// stale command in new receipts.
#[test]
fn the_resume_form_comes_from_the_capability_table() {
    let toml: std::collections::BTreeMap<String, toml::Value> =
        toml::from_str(crate::harness_capabilities::CAPABILITY_TOML).unwrap();
    for harness in ["claude", "codex"] {
        let tokens: Vec<String> = toml["harness"][&harness]["resume_strategy"]["forms"]
            ["interactive_resume"]["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().replace("{session_id}", "s-1"))
            .collect();
        let mut e = ask_row("form", None);
        e.harness = Some(harness.into());
        e.harness_session_id = Some("s-1".into());
        let receipt = build_reap_receipt(&e, None).unwrap();
        assert_eq!(receipt.resume, tokens.join(" "), "{harness}");
    }
    // A harness with no capability row (hermes hosts real sessions per
    // docs/SETUP-*.md and ships no row) cannot produce a resume command:
    // Unknown, by name. grok carried this fixture until x-fd31 landed
    // its row and the positive arm above covers the declared case.
    let mut e = ask_row("hermes-row", None);
    e.harness = Some("hermes".into());
    e.harness_session_id = Some("h-1".into());
    let err = build_reap_receipt(&e, None).unwrap_err();
    assert!(err.contains("hermes"), "{err}");
}

/// The ledger entry wins the enrichment (change 5): when the session
/// resolves there, the receipt carries node/pr/plan alongside the row's
/// own fields. The lookup answers None for a session the ledger never
/// recorded (the 12-of-26 population), and a `sessions` field that is
/// not an array never matches.
#[test]
fn the_ledger_entry_enriches_the_receipt_when_one_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.json");
    std::fs::write(
        &path,
        json!({
            "entries": [
                {
                    "graph_node_id": "x-abc1",
                    "pr_number": 1325,
                    "pr_url": "https://github.com/o/r/pull/1325",
                    "plan_path": "/plans/x-abc1.md",
                    "sessions": ["s-ledger"],
                },
                {"sessions": "s-ledger-impostor"},
            ]
        })
        .to_string(),
    )
    .unwrap();

    let rows = ledger_rows(&path).expect("ledger parses");
    let row = ledger_entry_in(&rows, "s-ledger").expect("the session resolves");
    assert_eq!(row["graph_node_id"], "x-abc1");
    assert!(ledger_entry_in(&rows, "s-never-recorded").is_none());
    // The impostor row carries `sessions` as a string: never matched.
    assert_eq!(ledger_entry_in(&rows, "s-ledger-impostor"), None);

    let mut e = ask_row("shipped", None);
    e.harness_session_id = Some("s-ledger".into());
    let receipt = build_reap_receipt(&e, Some(row)).unwrap();
    let led = receipt.ledger.expect("ledger enrichment present");
    assert_eq!(led["pr_number"], 1325);
}

/// Durability ordering, enforced not narrated: a receipt that cannot be
/// written holds its row in the registry. The receipts dir is pre-created
/// as a FILE, so the persist write fails and the sweep keeps the row.
#[test]
fn a_row_reaps_only_after_its_receipt_is_durable() {
    let home = tmp_home("gc-receipt-durability");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    std::fs::write(home.root().join("reap-receipts"), b"not a directory").unwrap();
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("undeliverable", Some(exited_at.as_str()));
        e.short_id = "undeliv".into();
        e.pid = Some(999_999_999); // confirmed dead: the policy would reap
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert!(summary.reaped.is_empty());
    assert!(
        summary
            .kept_no_receipt
            .iter()
            .any(|(id, reason)| id == "undeliv" && reason.contains("receipt did not persist")),
        "the persist failure must be surfaced: {:?}",
        summary.kept_no_receipt
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.name == "undeliverable"));
}

// A codex THREAD row (the x-6678 shape): harness codex, interactive host,
// no claude short id, no pane of its own - the row that owns no pid by
// design, because the shared daemon's pid is not the worker's to hold.
fn codex_thread_row(name: &str, exited_at: Option<&str>) -> RegistryEntry {
    let mut row = ask_row(name, exited_at);
    row.harness = Some("codex".into());
    row.host_mode = Some(state::HOST_MODE_INTERACTIVE.into());
    row
}

/// A stopped codex thread row whose rollout still exists must NOT be
/// corroborated-removable. PR 1255 correctly stopped recording a pid on
/// this row shape, which silently emptied `liveness_surface` for every
/// thread row and left `removal_is_corroborated` true on the
/// `!liveness_surface` arm alone: reapable with no probe, no transcript
/// read and no session check, while the thread is alive and resumable on
/// the daemon and the row is the only pointer back to it. The store probe
/// here answers the way codex's own rollout store does for a LIVE thread
/// (the session exists, the transcript is fresh), so the ONLY thing that
/// could reap this row is the missing-liveness arm.
#[test]
fn gc_sweep_keeps_a_stopped_codex_thread_row_whose_rollout_still_exists() {
    let home = tmp_home("gc-codex-thread");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    // Fresh mtime: inside the 1h grace, so transcript_fresh reads true.
    let rollout = home.root().join("rollout-cx-thread.jsonl");
    std::fs::write(&rollout, "{}\n").unwrap();
    state::update_registry(&home.registry_json(), |r| {
        r.entries
            .push(codex_thread_row("cx-thread", Some(exited_at.as_str())));
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|e: &state::RegistryEntry| match e.harness_session_id.as_deref() {
            Some("cx-thread-sess") => Some(vec![rollout.clone()]),
            _ => None,
        },
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert!(
        !summary.reaped.contains(&"cx-thread".to_string()),
        "a live codex thread's row is the only pointer back to the resumable session; {:?}",
        summary.reaped
    );
    assert_eq!(
        summary.kept_uncorroborated,
        vec!["cx-thread".to_string()],
        "kept, and the diagnostic names why"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        reg.entries.iter().any(|e| e.name == "cx-thread"),
        "the row itself stays on disk"
    );
}

/// The liveness term keys on ROW SHAPE, never on "has a session id". A
/// codex ONE-SHOT ask (host_mode exec) records a `harness_session_id`
/// naming a FINISHED exchange; widening the term there hands it a surface
/// nothing probes, which strands it instead of reaping it.
#[test]
fn gc_sweep_still_reaps_a_codex_one_shot_ask_row_despite_a_session_id() {
    let home = tmp_home("gc-codex-ask");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("cx-ask-done", Some(exited_at.as_str()));
        e.harness = Some("codex".into());
        e.host_mode = Some(state::HOST_MODE_EXEC.into());
        r.entries.push(e);
    })
    .unwrap();

    // Past grace, short of the backstop horizon, and the ONLY corroboration
    // is the row's own store answering that the session is gone from it.
    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| Some(Vec::new()),
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert_eq!(
        summary.reaped,
        vec!["cx-ask-done".to_string()],
        "a finished one-shot ask with a session id must stay reapable"
    );
}

// -- The harness-store corroboration seam (AC1 / AC3 / AC5) -------------

#[test]
fn harness_store_keying_never_judges_one_harness_by_anothers_store() {
    // THE AC3 SPECIMEN: a codex worker has no claude transcript by
    // construction. Judged by claude's store it reads "session gone" and
    // reaps; judged by its own (empty) codex store it also reads gone -
    // but the keying is what makes each answer belong to the right row,
    // and an unknown harness must answer None (unjudgeable), never borrow
    // either store.
    let dir = tempfile::tempdir().expect("tmpdir");
    let claude_root = dir.path().join("claude-projects");
    let codex_root = dir.path().join("codex-sessions");
    let project = claude_root.join("-proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("sid-1234.jsonl"), "{}\n").unwrap();
    std::fs::create_dir_all(&codex_root).unwrap();

    let row_for = |harness: Option<&str>, sid: Option<&str>| RegistryEntry {
        node: None,
        harness: harness.map(str::to_string),
        harness_session_id: sid.map(str::to_string),
        ..ask_row("keying", None)
    };
    let mut idx = HarnessStoreIndex::with_roots(claude_root.clone(), codex_root.clone());

    let claude = idx
        .matches(&row_for(Some("claude"), Some("sid-1234")))
        .expect("claude store is readable, so it answers");
    assert_eq!(claude.len(), 1, "the session exists in claude's own store");

    let codex = idx
        .matches(&row_for(Some("codex"), Some("sid-1234")))
        .expect("codex store is readable, so it answers");
    assert!(
        codex.is_empty(),
        "the codex store holds no such session: its own answer, independent of claude's"
    );

    for unknown in ["gemini", "opencode", "agy"] {
        assert!(
            idx.matches(&row_for(Some(unknown), Some("sid-1234")))
                .is_none(),
            "an unknown harness ({unknown}) is unjudgeable, never judged by another store"
        );
    }
    // A row with no resolvable identity at all -- harness AND the legacy
    // provider fallback both blank -- is unjudgeable too. Distinct from
    // `harness: Some("")` alone: harness_name() treats a blank `harness`
    // the same as `None` and falls back to legacy_provider (tested below,
    // where the fixture's legacy_provider is "claude"), so this case must
    // blank the fallback too or it silently resolves to a known store.
    let blank = RegistryEntry {
        node: None,
        harness: Some(String::new()),
        harness_session_id: Some("sid-1234".to_string()),
        legacy_provider: String::new(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        ..ask_row("keying", None)
    };
    assert!(
        idx.matches(&blank).is_none(),
        "no resolvable harness identity at all is unjudgeable"
    );
    // No session id on the row: unjudgeable even for a known harness.
    assert!(idx.matches(&row_for(Some("claude"), None)).is_none());
    // A row whose harness falls back to the legacy provider field keys the
    // same way (harness_name resolves the alias).
    let mut legacy = HarnessStoreIndex::with_roots(claude_root, codex_root);
    let hits = legacy
        .matches(&row_for(None, Some("sid-1234")))
        .expect("claude (via legacy provider) store is readable, so it answers");
    assert_eq!(hits.len(), 1);
}

/// AC1 end-to-end: an exited row past grace whose harness session is gone
/// from its own store reaps, with the event carrying harness + session id
/// (the resumable/diagnostic handle), and the registry reap is never
/// blocked by a cascade that finds nothing to remove.
#[test]
fn gc_sweep_reaps_on_a_gone_harness_session_and_records_the_handle() {
    let home = tmp_home("gc-harness-gone");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("gone-session", Some(exited_at.as_str()));
        e.short_id = "gonesess".into(); // liveness surface, no live socket
        e.log_path = None; // the dead-evidence file that no longer exists
        e.harness = Some("claude".into());
        e.harness_session_id = Some("80e70ab4-1111".into());
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails, // truth tail: no live rows to probe
        // Session gone from its own store: the empty hit vector.
        &|e| (e.name == "gone-session").then(Vec::new),
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,          // cascade: store holds nothing to remove
    );

    assert_eq!(summary.reaped, vec!["gonesess".to_string()]);
    assert!(summary.cascade_refused.is_empty());
    // POSITIVE MARKER: the row is absent from the registry after the call,
    // not merely absent from the error output.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        reg.entries.iter().all(|e| e.name != "gone-session"),
        "the reap must be observable in the registry itself"
    );
    // The event carries the resumable handle fields.
    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(
        events.contains("\"harness_session_id\"") && events.contains("80e70ab4-1111"),
        "agent_row_reaped must record the harness session handle"
    );
}

#[test]
fn gc_sweep_node_session_clears_before_emitting_reap() {
    let home = tmp_home("gc-node-session-success");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("target-x-292e-worker", Some(exited_at.as_str()));
        e.short_id = "node-success".into();
        e.harness = Some("codex".into());
        e.harness_session_id = Some("codex-session-292e".into());
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl_with_node_cascade(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| Some(Vec::new()),
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        Some(&|entry, node_id, harness, session_id| {
            assert_eq!(entry.name, "target-x-292e-worker");
            assert_eq!(
                (node_id, harness, session_id),
                ("x-292e", "codex", "codex-session-292e")
            );
            Ok(NodeSessionCascadeReceipt {
                row_removed: true,
                status_before: Some("in_progress".into()),
                status_after: Some("ready".into()),
                remaining_open_do: 0,
            })
        }),
        &|_| None,
    );

    assert_eq!(summary.reaped, vec!["node-success".to_string()]);
    assert!(summary.node_session_refused.is_empty());
    let events = read_events(&home);
    let reap = events
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("agent_row_reaped"))
        .expect("positive reap event");
    assert_eq!(reap["data"]["node_session_cleared"], true);
    assert_eq!(reap["data"]["node_row_removed"], true);
    assert_eq!(reap["data"]["node_status_after"], "ready");
    assert_eq!(reap["data"]["node_remaining_open_do"], 0);
}

#[test]
fn gc_sweep_node_session_refusal_restores_registry() {
    let home = tmp_home("gc-node-session-refused");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("target-x-292e-refuse", Some(exited_at.as_str()));
        e.short_id = "node-refused".into();
        e.harness = Some("codex".into());
        e.harness_session_id = Some("codex-session-refused".into());
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl_with_node_cascade(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| Some(Vec::new()),
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        Some(&|_, _, _, _| Err("node read-back failed".into())),
        &|_| panic!("harness cascade must be skipped after node refusal"),
    );

    assert!(summary.reaped.is_empty());
    assert_eq!(
        summary.node_session_refused,
        vec![("node-refused".into(), "node read-back failed".into())]
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.name == "target-x-292e-refuse"));
    let events = read_events(&home);
    assert!(events
        .iter()
        .all(|event| { event.get("type").and_then(Value::as_str) != Some("agent_row_reaped") }));
    assert!(events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("daemon_recovery_error")
            && event["data"]["op"] == "node_session_refused"
    }));
}

/// AC4 end-to-end: an Orphaned row earns an exit stamp on first sight and
/// reaps once past grace with corroboration - the immortal-row mechanism.
#[test]
fn gc_sweep_orphaned_row_stamps_then_reaps() {
    let home = tmp_home("gc-orphaned");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");

    // First pass: unstamped Orphaned -> StampExit, nothing removed.
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("orph", None);
        e.status = AgentStatus::Orphaned;
        e.short_id = "orph".into();
        e.log_path = None;
        r.entries.push(e);
    })
    .unwrap();
    let first = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );
    assert!(first.reaped.is_empty(), "first sight stamps, never reaps");
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let stamped = reg
        .entries
        .iter()
        .find(|e| e.name == "orph")
        .expect("row survives the stamping pass")
        .exited_at
        .clone();
    assert!(
        stamped.is_some(),
        "an unreachable probe is an observation: the clock starts"
    );

    // Backdate past grace, corroborate, sweep again: reaped.
    state::update_registry(&home.registry_json(), |r| {
        for e in r.entries.iter_mut() {
            if e.name == "orph" {
                e.exited_at = Some(exited_at.clone());
            }
        }
    })
    .unwrap();
    let second = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|e| (e.name == "orph").then(Vec::new),
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );
    assert_eq!(second.reaped, vec!["orph".to_string()]);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().all(|e| e.name != "orph"));
}
mod gc_dormant_tests;

/// AC6: a reaped row whose harness session refuses removal keeps the
/// refusal in the report (surfaced, never swallowed), and the registry reap
/// is NOT rolled back. Cascade injected so the staged refusal is
/// deterministic - no PATH/HOME games against a parallel test run.
#[test]
fn gc_sweep_cascade_refusal_is_surfaced_and_never_undoes_the_reap() {
    let home = tmp_home("gc-cascade");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = ask_row("pid-dead", Some(exited_at.as_str()));
        e.short_id = "piddead".into();
        e.log_path = None;
        e.harness = Some("codex".into());
        e.harness_session_id = Some("sid-cascade".into());
        e.pid = Some(999_999_999); // no such process: confirmed dead
        r.entries.push(e);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        // The cascade refuses for this row: the harness store would not
        // give the session up.
        &|e| {
            (e.name == "pid-dead").then(|| {
                (
                    "piddead".to_string(),
                    "cascade refused (staged)".to_string(),
                )
            })
        },
    );

    assert_eq!(summary.reaped, vec!["piddead".to_string()]);
    assert_eq!(
        summary.cascade_refused,
        vec![(
            "piddead".to_string(),
            "cascade refused (staged)".to_string()
        )],
        "the refusal is surfaced with its reason"
    );
    // POSITIVE MARKER: the registry itself no longer holds the row - the
    // refusal never rolled back the reap.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().all(|e| e.name != "pid-dead"));
}

/// The codex cascade core: index surgery drops only the matching session's
/// entry, keeps unparsable lines, and reports nothing to remove as a no-op.
#[test]
fn codex_index_cascade_drops_only_the_matching_entry() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let index = dir.path().join("session_index.jsonl");
    std::fs::write(
        &index,
        concat!(
            "{\"session_id\":\"aaa\"}\n",
            "not-json-at-all\n",
            "{\"session_id\":\"bbb\"}\n",
        ),
    )
    .unwrap();
    assert_eq!(cascade_codex_index(&index, "missing", "row"), Ok(false));
    let after_noop = std::fs::read_to_string(&index).unwrap();
    assert_eq!(
        after_noop.lines().count(),
        3,
        "no match: byte-for-byte no-op"
    );

    assert_eq!(cascade_codex_index(&index, "aaa", "row"), Ok(true));
    let after = std::fs::read_to_string(&index).unwrap();
    assert!(!after.contains("\"aaa\""), "the matching entry is gone");
    assert!(after.contains("\"bbb\""), "other entries stay");
    assert!(
        after.contains("not-json-at-all"),
        "an unparsable line is never destroyed"
    );

    // A missing index is a no-op, not a refusal.
    assert_eq!(
        cascade_codex_index(&dir.path().join("nope.jsonl"), "aaa", "row"),
        Ok(false)
    );
}

/// `--dry-run` (x-9de7 task 5): the same classification as a real sweep,
/// including the kept-reason diagnostics, but the registry is provably
/// untouched and no `agent_row_reaped` event lands.
#[test]
fn gc_sweep_dry_run_reports_without_mutating() {
    let home = tmp_home("gc-dry-run");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let recent_exit = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        // Would reap (no liveness surface -> auto-corroborated regardless
        // of age, so an old stamp is fine here).
        r.entries
            .push(ask_row("ask-old", Some("2020-01-01T00:00:00Z")));
        // Would keep uncorroborated: past grace, short of the 7-day
        // backstop horizon.
        let mut stuck = ask_row("stuck", Some(recent_exit.as_str()));
        stuck.short_id = "stuck".into();
        stuck.log_path = None;
        r.entries.push(stuck);
    })
    .unwrap();
    let before = state::load_registry(&home.registry_json()).unwrap();

    // gc_sweep_impl directly, not the gc_sweep_dry_run wrapper: see the
    // comment on gc_sweep_reports_kept_uncorroborated_for_the_stuck_and_invisible_row
    // for why the wrapper's real-$HOME HarnessStoreIndex is not hermetic.
    let emitter = EventEmitter::new(std::path::PathBuf::new(), "daemon");
    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        true,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert_eq!(summary.reaped, vec!["ask-old".to_string()]);
    assert_eq!(summary.kept_uncorroborated, vec!["stuck".to_string()]);

    let after = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(
        before.entries.len(),
        after.entries.len(),
        "dry-run must not remove a row"
    );
    assert!(
        after.entries.iter().any(|e| e.name == "ask-old"),
        "dry-run must not remove ask-old from disk"
    );
    assert!(
        !std::path::Path::new(&home.events_jsonl()).exists(),
        "dry-run must never emit agent_row_reaped"
    );
}

/// The long-silence repro (x-9de7 verification #7, the one task 6 exists
/// for). A codex row whose transcript went untouched for 90 minutes while
/// the pane was alive: under the OLD one-hour-for-every-harness window
/// that silence corroborates a reap; under an 8h codex grace it does not.
/// Both sweeps run against the SAME fixture (same exited_at, same
/// transcript mtime) so the only variable is the grace the resolver hands
/// back for "codex".
#[test]
fn gc_sweep_a_90_minute_codex_silence_reaps_under_1h_grace_not_under_8h() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let exited_at_secs = now - 9 * 3600; // well past either grace
    let (y, mo, d, h, mi, s) = civil(exited_at_secs);
    let exited_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");

    let seed = |tag: &str| -> (AgentsHome, std::path::PathBuf) {
        let home = tmp_home(tag);
        let log_path = home.root().join("transcript.jsonl");
        std::fs::write(&log_path, "{}\n").unwrap();
        let mtime =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(now - 90 * 60);
        std::fs::File::options()
            .write(true)
            .open(&log_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        state::update_registry(&home.registry_json(), |r| {
            let mut e = ask_row("codex-silent", Some(exited_at.as_str()));
            e.short_id = "codex-silent".into(); // liveness_surface, no live socket
            e.harness = Some("codex".into());
            e.log_path = Some(log_path.to_string_lossy().into_owned());
            r.entries.push(e);
        })
        .unwrap();
        (home, log_path)
    };

    // gc_sweep_impl directly, not the gc_sweep wrapper: see the comment
    // on gc_sweep_reports_kept_uncorroborated_for_the_stuck_and_invisible_row
    // for why the wrapper's real-$HOME HarnessStoreIndex is not hermetic
    // - here it would auto-corroborate via harness_session_gone
    // regardless of the transcript-mtime freshness this test is about.

    // OLD behaviour: one number for every harness.
    let (home_old, _) = seed("gc-silence-old");
    let emitter_old = EventEmitter::new(home_old.events_jsonl(), "daemon");
    let summary_old = gc_sweep_impl(
        &home_old,
        &emitter_old,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );
    assert_eq!(
        summary_old.reaped,
        vec!["codex-silent".to_string()],
        "control: a 1h window reads 90 minutes of silence as corroborated staleness"
    );

    // FIXED behaviour: codex gets its own 8h grace/freshness window.
    let (home_new, _) = seed("gc-silence-new");
    let emitter_new = EventEmitter::new(home_new.events_jsonl(), "daemon");
    let summary_new = gc_sweep_impl(
        &home_new,
        &emitter_new,
        &|harness| Duration::from_secs(if harness == "codex" { 8 * 3600 } else { 3600 }),
        false,
        7,
        &no_tails,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );
    assert!(
        summary_new.reaped.is_empty(),
        "an 8h codex grace must not corroborate a worker that was silent for only 90 minutes"
    );
}

#[test]
fn gc_sweep_turns_unterminated_node_reap_into_durable_failure() {
    let sandbox = tmp_home("gc-dead-dispatch");
    let home = AgentsHome::at(sandbox.root().join("agents"));
    home.ensure_root().unwrap();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let dead_repo = home.root().join("dead-repo");
    let done_repo = home.root().join("done-repo");
    for repo in [&dead_repo, &done_repo] {
        std::fs::create_dir_all(repo.join(".fno")).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
    }

    let dead_session = "target-run-dead";
    let done_session = "target-run-done";
    std::fs::write(
        dead_repo.join(".fno/target-state.md"),
        format!("---\nfno_id: {dead_session}\ninput: x-a35a\nplan_path: \"\"\n---\n"),
    )
    .unwrap();
    std::fs::write(
        done_repo.join(".fno/target-state.md"),
        format!("---\nfno_id: {done_session}\ninput: x-b44e\nplan_path: \"\"\n---\n"),
    )
    .unwrap();
    state::update_registry(&home.registry_json(), |r| {
        let mut dead = bg_claude_row("target-x-a35a-route-atomicity", "dead0001");
        dead.status = AgentStatus::Exited;
        dead.cwd = dead_repo.to_string_lossy().into_owned();
        dead.exited_at = Some("2020-01-01T00:00:00Z".into());
        dead.log_path = Some(stale_log(&dead_repo));
        dead.harness_session_id = Some("dead-harness-uuid".into());
        r.entries.push(dead);

        let mut done = bg_claude_row("target-x-b44e-finished", "done0002");
        done.status = AgentStatus::Exited;
        done.cwd = done_repo.to_string_lossy().into_owned();
        done.exited_at = Some("2020-01-01T00:00:00Z".into());
        done.log_path = Some(stale_log(&done_repo));
        done.harness_session_id = Some("done-harness-uuid".into());
        r.entries.push(done);
    })
    .unwrap();

    let global_events = home.root().parent().unwrap().join("events.jsonl");
    std::fs::write(
            done_repo.join(".fno/events.jsonl.1"),
            format!(
                "{{\"ts\":\"2026-07-24T00:00:00Z\",\"type\":\"termination\",\"source\":\"loop\",\"data\":{{\"session_id\":\"{done_session}\",\"reason\":\"DonePRGreen\",\"message\":\"done\"}}}}\n"
            ),
        )
        .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(0),
        false,
        7,
        &live_truth_tail_states,
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );
    assert_eq!(summary.reaped.len(), 2);

    let reaps = read_events(&home);
    // The reap event, by type: the removal accounting (x-a879) also emits
    // registry_row_removed into this log carrying the same short_id, so a
    // bare short_id match is ambiguous.
    let dead_reap = reaps
        .iter()
        .find(|e| e["type"] == "agent_row_reaped" && e["data"]["short_id"] == "dead0001")
        .expect("dead dispatch reap event");
    assert_eq!(dead_reap["data"]["node_id"], "x-a35a");
    assert_eq!(dead_reap["data"]["termination_event"], false);
    let done_reap = reaps
        .iter()
        .find(|e| e["type"] == "agent_row_reaped" && e["data"]["short_id"] == "done0002")
        .expect("completed dispatch reap event");
    assert_eq!(done_reap["data"]["node_id"], "x-b44e");
    assert_eq!(done_reap["data"]["termination_event"], true);

    let global = std::fs::read_to_string(&global_events).unwrap();
    let failures: Vec<Value> = global
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|e: &Value| e["type"] == "node_failed")
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["data"]["unit_id"], "x-a35a");
    assert_eq!(failures[0]["data"]["session_id"], dead_session);
    assert_eq!(
        failures[0]["data"]["reason"],
        "agent-row-reaped-no-termination"
    );
}

#[test]
fn gc_sweep_restores_row_when_termination_evidence_is_unknown() {
    let sandbox = tmp_home("gc-unknown-termination");
    let home = AgentsHome::at(sandbox.root().join("agents"));
    home.ensure_root().unwrap();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let repo = home.root().join("repo");
    std::fs::create_dir_all(repo.join(".fno")).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    std::fs::write(
        repo.join(".fno/target-state.md"),
        "---\nfno_id: reused-run\ninput: x-other\nplan_path: \"\"\n---\n",
    )
    .unwrap();
    state::update_registry(&home.registry_json(), |registry| {
        let mut row = bg_claude_row("target-x-a35a-route-atomicity", "dead0001");
        row.status = AgentStatus::Exited;
        row.cwd = repo.to_string_lossy().into_owned();
        row.exited_at = Some("2020-01-01T00:00:00Z".into());
        row.log_path = Some(stale_log(&repo));
        // A real bg worker carries its session identity, so the receipt
        // gate stages a record and the row reaches the dispatch path this
        // test exercises.
        row.harness_session_id = Some("019cdead-0000-7000-8000-000000000001".into());
        registry.entries.push(row);
    })
    .unwrap();

    let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(0), 7);

    assert!(summary.reaped.is_empty());
    let registry = state::load_registry(&home.registry_json()).unwrap();
    assert!(registry
        .entries
        .iter()
        .any(|row| row.name == "target-x-a35a-route-atomicity"));
    let events = read_events(&home);
    assert!(events.iter().any(|event| {
        event["type"] == "daemon_recovery_error"
            && event["data"]["op"] == "observe_dead_dispatch_termination"
    }));
}

#[test]
fn gc_sweep_restores_row_when_dead_dispatch_receipt_cannot_persist() {
    let sandbox = tmp_home("gc-dead-dispatch-write-failure");
    let home = AgentsHome::at(sandbox.root().join("agents"));
    home.ensure_root().unwrap();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let repo = home.root().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    state::update_registry(&home.registry_json(), |registry| {
        let mut row = bg_claude_row("target-x-a35a-route-atomicity", "dead0001");
        row.status = AgentStatus::Exited;
        row.cwd = repo.to_string_lossy().into_owned();
        row.exited_at = Some("2020-01-01T00:00:00Z".into());
        row.log_path = Some(stale_log(&repo));
        // Session identity present, so the receipt gate stages a record
        // and the sweep reaches the dead-dispatch write this test breaks.
        row.harness_session_id = Some("019cdead-0000-7000-8000-000000000001".into());
        registry.entries.push(row);
    })
    .unwrap();
    std::fs::create_dir_all(global_events_path(&home)).unwrap();

    let summary = gc_sweep(&home, &emitter, &|_| Duration::from_secs(0), 7);

    assert!(summary.reaped.is_empty());
    let registry = state::load_registry(&home.registry_json()).unwrap();
    assert!(registry
        .entries
        .iter()
        .any(|row| row.name == "target-x-a35a-route-atomicity"));
    let events = read_events(&home);
    assert!(events.iter().any(|event| {
        event["type"] == "daemon_recovery_error" && event["data"]["op"] == "record_dead_dispatch"
    }));
}

#[test]
fn recovery_emits_drive_crashed_before_clearing_window() {
    let home = tmp_home("recover-drive");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

    // Registry entry + state.json with a stale active drive window.
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: "worker-A".into(),
            short_id: "wkA".into(),
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            pid: Some(std::process::id()), // alive -> not reaped
            pid_start_time: None,
            keeper_child_pid: None,
            log_path: Some("/tmp/worker-A.log".into()), // x-7bcd: resolvable handle
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
            ..Default::default()
        });
    })
    .unwrap();
    let mut st = AgentState::new_pty("wkA");
    st.status = AgentStatus::Live;
    st.pty = Some(PtyState {
        active: true,
        drive: Some(DriveWindow {
            session_id: Some("drive-xyz".into()),
            mode: Some("interactive".into()),
            last_heartbeat_at_monotonic_ns: Some(123),
        }),
    });
    state::write_state_atomic(&home.state_json("wkA"), &st).unwrap();

    let report = recover(&home, &emitter).expect("startup recovery");
    assert_eq!(report.recovered_drives, vec!["wkA".to_string()]);

    // drive_crashed emitted, carrying the session id (proves read-before-clear).
    let events = read_events(&home);
    let crashed = events
        .iter()
        .find(|e| e["type"] == "drive_crashed")
        .expect("drive_crashed emitted");
    assert_eq!(crashed["data"]["session_id"], "drive-xyz");
    assert_eq!(crashed["data"]["reason"], "daemon_restart");

    // The on-disk state has the window cleared after recovery.
    let after = state::load_state(&home.state_json("wkA")).unwrap().unwrap();
    let pty = after.pty.unwrap();
    assert!(pty.drive.is_none());
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_marks_missing_state_inconsistent() {
    let home = tmp_home("recover-missing");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: "ghost".into(),
            short_id: "ghost".into(),
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            keeper_child_pid: None,
            log_path: Some("/tmp/ghost.log".into()), // x-7bcd: resolvable handle
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
            ..Default::default()
        });
    })
    .unwrap();
    // No state.json written for "ghost".
    let report = recover(&home, &emitter).expect("startup recovery");
    assert_eq!(
        report.inconsistent,
        vec![("ghost".to_string(), InconsistencyReason::MissingStateJson)]
    );
    let events = read_events(&home);
    assert!(events
        .iter()
        .any(|e| e["type"] == "agent_inconsistent" && e["data"]["reason"] == "missing_state_json"));
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_skips_claude_shellout_rows_no_spurious_inconsistent() {
    // x-1b1e regression: v9 gives a claude `--bg`/`ask` row a non-empty
    // short_id (the jobId), and an adopted row keeps its external pid. Neither
    // has an fno do state.json (their process is claude's, not a daemon PTY), so
    // recover() must NOT probe state_json(jobId) and emit a spurious
    // agent_inconsistent -- the empty-short_id proxy no longer catches them.
    let home = tmp_home("recover-claude-shellout");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        // bg/ask: host_mode exec (None), pid None.
        let mut bg = bg_claude_row("bg-ask", "7c5dcf5d");
        bg.host_mode = None;
        r.entries.push(bg);
        // adopted: host_mode attached, external pid set.
        let mut adopted = bg_claude_row("cc-adopt", "deadbeef");
        adopted.host_mode = Some(crate::state::HOST_MODE_ATTACHED.into());
        adopted.pid = Some(4242);
        r.entries.push(adopted);
    })
    .unwrap();
    // No state.json written for either row.
    let report = recover(&home, &emitter).expect("startup recovery");
    assert!(
        report.inconsistent.is_empty(),
        "claude shellout/adopted rows must not be flagged inconsistent: {:?}",
        report.inconsistent
    );
    let events = read_events(&home);
    assert!(
        !events.iter().any(|e| e["type"] == "agent_inconsistent"),
        "no agent_inconsistent event for claude shellout rows"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn canonical_name_in_resolves_all_three_address_forms() {
    // x-1b1e regression: the daemon stop/rm handlers must accept name |
    // 8-hex short | full session id (parity with Python `_canonical_agent_name`),
    // not just the name. A miss falls back to the raw token so the familiar
    // `agent {name} not found` still fires.
    let full = "aabbccdd-1111-2222-3333-444455556666";
    let mut row = rentry("billing", AgentStatus::Live, None);
    row.short_id = "a1b2c3d4".into();
    row.harness_session_id = Some(full.into());
    let reg = crate::state::Registry {
        schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
        entries: vec![row],
    };
    assert_eq!(canonical_name_in(&reg, "billing"), "billing"); // by name
    assert_eq!(canonical_name_in(&reg, "a1b2c3d4"), "billing"); // by stored short
    assert_eq!(canonical_name_in(&reg, full), "billing"); // by full session id
    assert_eq!(
        canonical_name_in(&reg, "AABBCCDD-1111-2222-3333-444455556666"),
        "billing"
    ); // case-insensitive
       // Unknown token -> unchanged, so the caller's not-found path fires.
    assert_eq!(canonical_name_in(&reg, "nope"), "nope");
}

#[tokio::test]
async fn lifecycle_name_resolution_never_falls_back_on_ambiguity() {
    let mut named = rentry("deadbeef", AgentStatus::Live, None);
    named.short_id = "transport-a".into();
    named.harness_session_id = Some("aaaaaaaa-1111-2222-3333-444455556666".into());
    let mut short = rentry("other", AgentStatus::Live, None);
    short.short_id = "deadbeef".into();
    short.harness_session_id = Some("bbbbbbbb-1111-2222-3333-000000000002".into());
    let reg = crate::state::Registry {
        schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
        entries: vec![named, short],
    };

    let error = entry_for_lifecycle(
        &reg,
        "deadbeef",
        std::path::Path::new("/nonexistent/registry.json"),
    )
    .await
    .expect_err("ambiguous token must not fall back to the matching row name");

    assert!(error.contains("ambiguous across 2 agents"));
}

#[test]
fn recovery_reaps_dead_pid() {
    let home = tmp_home("recover-reap");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: "dead".into(),
            short_id: "dead".into(),
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            // PID 2^31-ish: almost certainly not a live process.
            pid: Some(0x7fff_fff0),
            pid_start_time: None,
            keeper_child_pid: None,
            log_path: Some("/tmp/dead.log".into()), // x-7bcd: resolvable handle
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
            ..Default::default()
        });
    })
    .unwrap();
    // Give it a state.json so it isn't flagged inconsistent.
    let mut st = AgentState::new_pty("dead");
    st.status = AgentStatus::Live;
    state::write_state_atomic(&home.state_json("dead"), &st).unwrap();

    let report = recover(&home, &emitter).expect("startup recovery");
    assert_eq!(report.reaped_pids, vec![0x7fff_fff0]);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.find("dead").unwrap().status, AgentStatus::Exited);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_marks_dead_interactive_exited_and_preserves_host_mode() {
    // AC2-FR (task 2.3): a genuinely dead interactive worker is reaped to
    // Exited (the design's "unexpected exit is exited, not orphaned"), and
    // its host_mode="interactive" round-trips through recovery unchanged so
    // a daemon restart that rediscovers it keeps the field.
    let home = tmp_home("recover-interactive");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        let mut e = rentry("hosted", AgentStatus::Live, None);
        e.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        e.pid = Some(0x7fff_fff0); // not a live process
        e.log_path = Some("/tmp/hosted.log".into()); // x-7bcd: resolvable handle
        r.entries.push(e);
    })
    .unwrap();
    let mut st = AgentState::new_pty("hosted");
    st.status = AgentStatus::Live;
    state::write_state_atomic(&home.state_json("hosted"), &st).unwrap();

    let _ = recover(&home, &emitter);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let row = reg.find("hosted").unwrap();
    assert_eq!(
        row.status,
        AgentStatus::Exited,
        "a dead interactive worker is exited, never orphaned"
    );
    assert_eq!(
        row.host_mode_or_default(),
        crate::state::HOST_MODE_INTERACTIVE,
        "host_mode must survive recovery"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_orphan_pid_sweep_does_not_condemn_every_row_sharing_an_empty_short_id() {
    // x-9de7 task 1: the orphan-PID sweep (Step 6 of recover(), ~line 270)
    // collects reaped short_ids into a `BTreeSet<String>`, then marks EVERY
    // entry whose short_id is a MEMBER of that set as Exited -- not just the
    // specific entry that failed pid_is_ours. Every codex/gemini shellout
    // row shares the same empty short_id (see the comment at the top of
    // recover()), so one genuinely dead pane-hosted row poisons every live
    // one that happens to sit beside it in the registry. This is the writer
    // behind the false `exited` write on a live mux pane row.
    let home = tmp_home("recover-empty-short-id-collision");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    let Some(my_start) = process_start_time(me) else {
        return; // platform without start-time support; nothing to assert
    };
    state::update_registry(&home.registry_json(), |r| {
        // Genuinely dead: pid_is_ours must return false for this one.
        let mut dead = ask_row("dead-pane", None);
        dead.status = AgentStatus::Live;
        dead.pid = Some(0x7fff_fff0); // not a live process
        r.entries.push(dead);

        // Live: real pid, matching start time, hosted in a mux pane -- same
        // empty short_id as the dead row above.
        let mut live = ask_row("live-pane", None);
        live.status = AgentStatus::Live;
        live.pid = Some(me);
        live.pid_start_time = Some(my_start);
        live.mux = Some(state::MuxRef {
            session: "main".into(),
            pane_id: 1,
        });
        r.entries.push(live);
    })
    .unwrap();

    let _ = recover(&home, &emitter);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    let live = reg.find("live-pane").unwrap();
    assert_eq!(
        live.status,
        AgentStatus::Live,
        "a live pane-hosted row must not be condemned by a sibling's empty short_id"
    );
    assert!(
        live.pid.is_some(),
        "the writer clears no pid; a fix must not start clearing it here either"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn pid_is_ours_distinguishes_recycled_pid() {
    // ab-d19e6458: a live pid whose start time no longer matches the recorded
    // one is a recycled pid, not our worker.
    let me = std::process::id();
    let Some(st) = process_start_time(me) else {
        return; // platform without start-time support; nothing to assert
    };
    assert!(pid_is_ours(me, Some(st)), "correct start time -> ours");
    assert!(
        !pid_is_ours(me, Some(st.wrapping_add(1))),
        "alive but mismatched start time -> recycled, not ours"
    );
    assert!(
        !pid_is_ours(0x7fff_fff0, Some(st)),
        "dead pid is never ours"
    );
    assert!(
        pid_is_ours(me, None),
        "no recorded start time -> fall back to bare liveness (legacy)"
    );
}

// ---- x-cd31: idle-exit reads live workers, not registry emptiness ------

/// A row with a live-pid shape (short_id set, pid + matching start time),
/// the row that must PIN the daemon.
fn live_pid_row(short_id: &str) -> RegistryEntry {
    let mut row = ask_row(short_id, None);
    row.short_id = short_id.to_string();
    row.pid = Some(std::process::id());
    row.pid_start_time = process_start_time(std::process::id());
    row
}

#[test]
fn idle_exit_fires_on_terminal_rows_with_no_live_worker() {
    // The exact defect box: a registry of TERMINAL rows (the roster an
    // established machine always has) with no live socket and no live pid
    // must let the daemon exit. Registry emptiness never held here.
    let home = short_home("idle-terminal");
    home.ensure_root().unwrap();
    state::update_registry(&home.registry_json(), |r| {
        r.entries
            .push(ask_row("done-1", Some("2020-01-01T00:00:00Z")));
        r.entries
            .push(ask_row("done-2", Some("2020-01-01T00:00:00Z")));
    })
    .unwrap();
    assert!(
        no_live_worker(&home),
        "terminal rows with dead pids and no sockets must not pin the daemon"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn idle_exit_held_by_a_live_worker_socket() {
    let home = short_home("idle-sock");
    home.ensure_root().unwrap();
    std::fs::create_dir_all(home.agent_dir("wka")).unwrap();
    // A REAL listener: since the stale-socket fix, file existence alone
    // does not pin the daemon - something must answer on the socket.
    let _listener = std::os::unix::net::UnixListener::bind(home.worker_sock("wka")).unwrap();
    state::update_registry(&home.registry_json(), |r| {
        // Terminal row, dead pid, but a live worker serving on its socket.
        let mut row = ask_row("wka", Some("2020-01-01T00:00:00Z"));
        row.short_id = "wka".to_string();
        r.entries.push(row);
    })
    .unwrap();
    assert!(
        !no_live_worker(&home),
        "a reachable worker socket pins the daemon regardless of what its row says"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn idle_exit_not_held_by_a_stale_socket_file() {
    // A worker killed without reaping its socket leaves the FILE behind.
    // Nothing answers on it, so it must not pin the daemon - the exact
    // stale-file case the connect probe exists for (a pid-less live row
    // would otherwise make the daemon immortal).
    let home = short_home("idle-stale");
    home.ensure_root().unwrap();
    std::fs::create_dir_all(home.agent_dir("wka")).unwrap();
    std::fs::write(home.worker_sock("wka"), b"").unwrap();
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("wka", None);
        row.short_id = "wka".to_string();
        r.entries.push(row);
    })
    .unwrap();
    assert!(
        no_live_worker(&home),
        "a socket file nobody answers on is not a live worker"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn idle_exit_held_by_a_live_worker_pid() {
    let home = short_home("idle-pid");
    home.ensure_root().unwrap();
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(live_pid_row("wkb"));
    })
    .unwrap();
    assert!(
        !no_live_worker(&home),
        "a row whose pid is still ours pins the daemon"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn idle_exit_held_by_an_unreadable_registry() {
    // The fail-safe side: an unreadable registry is an absence with two
    // explanations, and the daemon must stay resident rather than exit on
    // a transient read failure (the old code exited: unwrap_or(true)).
    let home = short_home("idle-unreadable");
    home.ensure_root().unwrap();
    std::fs::write(home.registry_json(), "not json at all{").unwrap();
    assert!(
        !no_live_worker(&home),
        "an unreadable registry must not license an idle exit"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn idle_exit_fires_on_a_fresh_home_with_no_registry() {
    // The first daemon on a fresh machine: no registry file has ever been
    // written, and lazy-exit must hold for it too (the missing file is
    // "nothing ever tracked", not a read failure).
    let home = short_home("idle-fresh");
    home.ensure_root().unwrap();
    assert!(
        no_live_worker(&home),
        "a fresh home with no registry must idle-exit"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn daemon_exited_payload_distinguishes_socket_loss() {
    // x-3498 AC: an abnormal retirement (socket path taken from under us)
    // must read differently from a graceful ending.
    assert_eq!(
        daemon_exited_payload("socket-lost"),
        json!({"clean": false, "reason": "socket-lost"})
    );
    assert_eq!(
        daemon_exited_payload("sigterm"),
        json!({"clean": true, "reason": "sigterm"})
    );
    assert_eq!(
        daemon_exited_payload("idle"),
        json!({"clean": true, "reason": "idle"})
    );
}

#[tokio::test]
async fn stop_claude_pid_kills_a_real_child_and_spares_a_recycled_pid() {
    // x-a4b2: a row with a pid and no transport id must actually be stopped
    // (it used to be refused, leaving a live duplicate worker), and a pid
    // whose start time no longer matches must be left alone.
    let mut entry = ask_row("orphan", None);

    // A row with no pid at all has nothing to signal.
    assert!(
        !stop_claude_pid_confirmed(&entry).await,
        "no pid -> nothing to stop"
    );

    // Spawn the sleeper as a DETACHED grandchild: `sh` backgrounds it and
    // exits, so it is reparented away and is never this test's child. A
    // direct child would linger as a zombie after SIGTERM until reaped, and
    // `pid_is_ours` (a bare `kill(pid, 0)` probe) reads a zombie as alive.
    // The real claude worker is not the daemon's child either, so this also
    // matches production.
    let out = std::process::Command::new("sh")
        .arg("-c")
        // The redirect is load-bearing: a backgrounded child inherits sh's
        // stdout pipe, so without it `.output()` blocks for the full sleep
        // waiting on EOF instead of returning as soon as sh exits.
        .arg("sleep 60 >/dev/null 2>&1 & echo $!")
        .output()
        .expect("spawn detached sleeper");
    let pid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("sleeper pid");
    let start = process_start_time(pid);

    // Independent death oracle. Asserting with `pid_is_ours` would use the
    // subject's own probe as its judge, and that probe reports EPERM and a
    // recycled pid as not-ours too, so it can read "gone" over a process
    // that is still running. `ps` knows nothing about our guards.
    let ps_says_alive = |pid: u32| {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.split_whitespace().next() == Some(&pid.to_string()))
                    .count()
                    > 0
            })
            .unwrap_or(false)
    };

    // No incarnation token: bare liveness is not a licence to SIGKILL.
    entry.pid = Some(pid);
    entry.pid_start_time = None;
    assert!(
        !stop_claude_pid_confirmed(&entry).await,
        "no start token -> refuse"
    );
    assert!(ps_says_alive(pid), "a refused row must not be signalled");

    // Wrong incarnation token: the pid belongs to someone else now.
    if let Some(st) = start {
        entry.pid_start_time = Some(st.wrapping_add(1));
        assert!(
            !stop_claude_pid_confirmed(&entry).await,
            "recycled pid -> refuse"
        );
        assert!(
            ps_says_alive(pid),
            "an unrelated process must not be signalled"
        );
    }

    // Correct token: the process is really killed, not merely reported.
    entry.pid_start_time = start;
    if start.is_some() {
        assert!(
            stop_claude_pid_confirmed(&entry).await,
            "owned live pid -> stopped"
        );
        assert!(!ps_says_alive(pid), "process is gone");
    } else {
        // No readable start time on this platform: the guard above refuses
        // every row, so reap the sleeper rather than leaking it.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[test]
fn pid_is_ours_rejects_an_out_of_range_pid() {
    // u32::MAX wraps to -1 in signed pid_t, the "signal every process I may
    // signal" broadcast target. kill(-1, 0) succeeds and no start time is
    // readable, so without the range guard the probe returns true and the
    // caller broadcasts SIGTERM.
    assert!(!pid_is_ours(u32::MAX, None), "u32::MAX must never be ours");
    assert!(
        !pid_is_ours(i32::MAX as u32 + 1, Some(123)),
        "anything past i32::MAX wraps negative"
    );
    assert!(
        pid_confirmed_dead(u32::MAX),
        "out-of-range is never running"
    );
}

#[test]
fn recycle_and_death_each_demand_positive_evidence() {
    // The distinction `pid_gone_within` rests on. `!pid_is_ours` is NOT a
    // recycle test: it is also false for a live-but-unsignalable process, and
    // treating that as "gone" reports a clean stop over a running worker.
    let me = std::process::id();
    let Some(st) = process_start_time(me) else {
        return; // platform without start-time support
    };

    // Alive and ours: neither dead nor recycled.
    assert!(!pid_confirmed_dead(me), "a live pid is not dead");
    assert!(
        !pid_recycled(me, Some(st)),
        "matching token is not a recycle"
    );

    // Alive with a mismatched token: a positive recycle finding.
    assert!(
        pid_recycled(me, Some(st.wrapping_add(1))),
        "reachable + differing token is a recycle"
    );

    // No recorded token: no basis to claim a recycle either way.
    assert!(!pid_recycled(me, None), "no token -> no recycle verdict");

    // A dead pid is dead, and is never *also* reported as recycled -- the
    // caller must not be able to reach "gone" through an unproven path.
    let dead = 0x7fff_fff0u32;
    assert!(pid_confirmed_dead(dead), "unused high pid reads as dead");
    assert!(
        !pid_recycled(dead, Some(st)),
        "dead is not a recycle finding"
    );
}

#[test]
fn recovery_reaps_recycled_pid() {
    // ab-d19e6458: the recorded pid is ALIVE (our own), but its start time
    // does not match — the original worker died and the pid was reused by an
    // unrelated process. The reap must fire on the start-time mismatch, not
    // be fooled by bare liveness.
    let home = tmp_home("recover-recycled");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let me = std::process::id();
    if process_start_time(me).is_none() {
        std::fs::remove_dir_all(home.root()).ok();
        return; // start-time unsupported here; reuse detection N/A
    }
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(RegistryEntry {
            substrate: None,
            node: None,
            spawned_by_session: None,
            spawned_by_harness: None,
            spawned_by_cwd: None,
            launch_account: None,
            related_session_id: None,
            origin: None,
            name: "recycled".into(),
            short_id: "recycled".into(),
            legacy_provider: "codex".into(),
            provider: None,
            model: None,
            model_basis: None,
            effort: None,
            harness: None,
            harness_session_id: None,
            predecessor_session_ids: Vec::new(),
            forked_from_session_id: None,
            route_provider_id: None,
            model_name: None,
            account_record_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: None,
            spawn_trigger: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            pid: Some(me),
            // Bogus start time -> mismatch against our real one -> not ours.
            pid_start_time: Some(1),
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
            ..Default::default()
        });
    })
    .unwrap();
    let mut st = AgentState::new_pty("recycled");
    st.status = AgentStatus::Live;
    state::write_state_atomic(&home.state_json("recycled"), &st).unwrap();

    let report = recover(&home, &emitter).expect("startup recovery");
    assert_eq!(report.reaped_pids, vec![me]);
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.find("recycled").unwrap().status, AgentStatus::Exited);
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_archives_orphan_state_dir() {
    let home = tmp_home("recover-orphan");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    // A state dir with no registry entry.
    let mut st = AgentState::new_pty("loner");
    st.status = AgentStatus::Live;
    state::write_state_atomic(&home.state_json("loner"), &st).unwrap();

    let report = recover(&home, &emitter).expect("startup recovery");
    assert_eq!(report.archived_orphans, vec!["loner".to_string()]);
    assert!(!home.agent_dir("loner").exists(), "orphan dir moved aside");
    assert!(home.orphaned_dir().exists());
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_preserve_mode_keeps_orphan_state_dir() {
    let home = tmp_home("recover-preserve-orphan");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let mut st = AgentState::new_pty("loner");
    st.status = AgentStatus::Live;
    state::write_state_atomic(&home.state_json("loner"), &st).unwrap();

    let report = recover_with_policy(&home, &emitter, false).expect("preserve recovery");
    assert_eq!(report.recovery_mode, "preserve");
    assert!(report.archived_orphans.is_empty());
    assert!(
        home.agent_dir("loner").is_dir(),
        "preserve mode keeps the index"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_routes_codex_thread_by_full_identity_without_short_id() {
    let mut row = rentry("codex-thread", AgentStatus::Live, None);
    row.harness = Some("codex".into());
    row.legacy_provider.clear();
    row.short_id.clear();
    row.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.into());
    row.harness_session_id = Some("019f0000-0000-7000-8000-000000000001".into());
    row.codex_session_id = row.harness_session_id.clone();
    row.cwd = "/tmp/codex-thread-worktree".into();

    assert_eq!(
        codex_thread_resume_identity(&row).unwrap(),
        Some((
            "019f0000-0000-7000-8000-000000000001".into(),
            std::path::PathBuf::from("/tmp/codex-thread-worktree"),
        ))
    );
}

#[test]
fn recovery_refuses_codex_thread_when_identity_is_missing_by_name() {
    let mut row = rentry("codex-thread-missing", AgentStatus::Live, None);
    row.harness = Some("codex".into());
    row.legacy_provider.clear();
    row.short_id.clear();
    row.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.into());
    row.cwd = "/tmp/codex-thread-worktree".into();

    let error = codex_thread_resume_identity(&row).unwrap_err();
    assert!(error.contains("harness_session_id"), "error: {error}");

    row.harness_session_id = Some("019f0000-0000-7000-8000-000000000001".into());
    row.cwd.clear();
    let error = codex_thread_resume_identity(&row).unwrap_err();
    assert!(error.contains("cwd"), "error: {error}");
}

#[test]
fn recovery_skips_stopped_codex_thread_rows() {
    let mut row = rentry("codex-thread-stopped", AgentStatus::Exited, None);
    row.harness = Some("codex".into());
    row.legacy_provider.clear();
    row.short_id.clear();
    row.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.into());
    row.harness_session_id = Some("019f0000-0000-7000-8000-000000000009".into());
    row.codex_session_id = row.harness_session_id.clone();
    row.cwd = "/tmp/codex-thread-worktree".into();

    // The identity is complete, so resume WOULD be possible; the Exited
    // status from `fno agents stop` is what must veto the resurrection.
    assert!(codex_thread_resume_identity(&row).ok().flatten().is_some());
    assert!(!codex_thread_recovery_candidate(&row));

    row.status = AgentStatus::Live;
    assert!(codex_thread_recovery_candidate(&row));

    row.status = AgentStatus::PermanentDead;
    assert!(!codex_thread_recovery_candidate(&row));
}

#[test]
fn recovery_quarantines_and_reports_interrupted_write_temp() {
    let home = tmp_home("recover-interrupted-temp");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |_| {}).unwrap();
    let temp = home.root().join(".registry.json.tmp.75348");
    std::fs::write(&temp, b"partial").unwrap();

    let report = recover_with_policy(&home, &emitter, false).expect("temp recovery");
    assert_eq!(report.interrupted_write_temps.len(), 1);
    assert!(!temp.exists(), "the interrupted temp is not left in place");
    assert!(read_events(&home).iter().any(|e| {
        e["type"] == "daemon_recovery_interrupted_temp"
            && e["data"]["name"] == ".registry.json.tmp.75348"
    }));
    std::fs::remove_dir_all(home.root()).ok();
}

#[test]
fn recovery_does_not_quarantine_a_temp_held_by_an_active_writer() {
    let home = tmp_home("recover-active-temp");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |_| {}).unwrap();
    let temp = home.root().join(".registry.json.tmp.active");
    std::fs::write(&temp, b"partial").unwrap();
    let lock_path = std::path::PathBuf::from(format!("{}.lock", home.registry_json().display()));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.lock().unwrap();

    let found = quarantine_interrupted_write_temps(&home, &emitter);

    assert!(found.is_empty());
    assert!(temp.exists(), "active writer temp must remain in place");
}

#[test]
fn agent_name_validation() {
    assert!(state::is_valid_registry_label("worker-A_1"));
    assert!(!state::is_valid_registry_label(""));
    assert!(!state::is_valid_registry_label(&"x".repeat(65)));
    assert!(!state::is_valid_registry_label("has space"));
    assert!(!state::is_valid_registry_label("inject;rm"));
}

#[test]
fn uuid_v4_shape_and_uniqueness() {
    let a = uuid_v4();
    let b = uuid_v4();
    assert_ne!(a, b);
    assert_eq!(a.len(), 36);
    let parts: Vec<&str> = a.split('-').collect();
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12]
    );
    // version nibble is 4; variant nibble is 8/9/a/b.
    assert_eq!(&a[14..15], "4");
    assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
}

#[test]
fn short_id_derivation_dedups() {
    let mut reg = state::Registry::default();
    assert_eq!(derive_short_id("worker-A", &reg), "workerA");
    reg.entries.push(RegistryEntry {
        substrate: None,
        node: None,
        spawned_by_session: None,
        spawned_by_harness: None,
        spawned_by_cwd: None,
        launch_account: None,
        related_session_id: None,
        origin: None,
        name: "x".into(),
        short_id: "workerA".into(),
        legacy_provider: "codex".into(),
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: None,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        route_provider_id: None,
        model_name: None,
        account_record_id: None,
        cwd: "/".into(),
        project_root: "/".into(),
        session_id: None,
        spawn_trigger: None,
        legacy_claude_short_id: None,
        claude_session_uuid: None,
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: None,
        status: AgentStatus::Live,
        last_message_at: None,
        created_at: "t".into(),
        pid: None,
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
        ..Default::default()
    });
    assert_eq!(derive_short_id("worker-A", &reg), "workerA1");
}

// --- plan_reconcile (US6.9): tri-state, status-aware transitions, budget ---


fn claude_row(name: &str, uuid: &str, sid: &str) -> RegistryEntry {
    let mut e = rentry(name, AgentStatus::Idle, None);
    e.harness = Some("claude".into());
    e.claude_session_uuid = Some(uuid.into());
    e.harness_session_id = Some(sid.into());
    e
}

#[test]
fn a_ctrl_r_rename_emits_once_and_never_touches_the_label() {
    // (x-1ab9 task 5.1, AC10-HP) The sweep diffs the harness's title
    // against the row's last-seen value: first observation emits with
    // `from: null`, a repeat reads NO change, and the row's `name` is
    // never written from a title.
    let row = claude_row("w1", "uuid-1", "sid-1");
    let mut titles = std::collections::HashMap::new();
    titles.insert("uuid-1".to_string(), Some("renamed-by-ctrl-r".to_string()));

    let renames = title_changes(&[row.clone()], &titles);
    assert_eq!(
        renames,
        vec![(
            "w1".to_string(),
            Some("sid-1".to_string()),
            None,
            "renamed-by-ctrl-r".to_string()
        )]
    );

    let mut reg = state::Registry::default();
    reg.entries.push(row);
    let snapshot = reg.entries.clone();
    apply_title_changes(&mut reg, &snapshot, &titles);
    let stored = &reg.entries[0];
    assert_eq!(stored.harness_title.as_deref(), Some("renamed-by-ctrl-r"));
    assert_eq!(stored.name, "w1", "the label is never rewritten");

    // The next sweep sees the same title as no change: one emit, ever.
    let stored_row = stored.clone();
    assert!(
        title_changes(&[stored_row], &titles).is_empty(),
        "a settled title must not re-emit"
    );
}

#[test]
fn rename_emits_ride_the_successful_write() {
    // The emit loop sits AFTER the write-failure return inside
    // `run_reconcile_sweep`, so a failed write never announces a rename
    // it did not persist. Structural, so pin it like the budget clock.
    let src = include_str!("../../daemon.rs");
    let sweep = src
        .split("fn run_reconcile_sweep(")
        .nth(1)
        .expect("run_reconcile_sweep exists");
    let write = sweep
        .find("apply_title_changes(r, &entries, &titles)")
        .expect("title write");
    let fail = sweep.find("registry write failed").expect("failure return");
    let emit = sweep.find("\"agent_renamed\"").expect("rename emit");
    assert!(
        write < fail && fail < emit,
        "a failed write must return before any rename is emitted"
    );
}

#[test]
fn reconcile_budget_starts_after_truth_batch() {
    // (x-1ab9 task 4.1) The sweep's clock must start at plan_reconcile,
    // NOT before batched_row_truths: the truth batch serves every verb
    // and once ate the whole 5s budget (24s wall, 0 of 79 rows probed).
    // The budget's position is structural, so pin it where the source
    // cannot silently drift back: the clock line sits AFTER the truth
    // batch and the roster load inside `run_reconcile_sweep`.
    let src = include_str!("../../daemon.rs");
    let sweep = src
        .split("fn run_reconcile_sweep(")
        .nth(1)
        .expect("run_reconcile_sweep exists");
    let clock = sweep
        .find("let start = Instant::now();")
        .expect("clock line");
    let truth = sweep
        .find("batched_row_probes(&entries")
        .expect("truth batch call");
    let roster = sweep
        .find("ClaudeRoster::load_default()")
        .expect("roster load");
    assert!(
        truth < clock && roster < clock,
        "the sweep budget must start after the truth batch and roster load"
    );
}

#[test]
fn sweep_serves_liveness_over_a_stale_status_ac7_edge() {
    // A row whose stored status still reads `orphaned` (the t-x30c2-w1
    // shape) but whose probe says reachable serves `alive` with a fresh
    // stamp: the wire word comes from the measurement, never the status.
    let entries = vec![rentry("t-x30c2-w1", AgentStatus::Orphaned, None)];
    let (changes, _out) = plan_reconcile(
        &entries,
        |_| Ok(true),
        || false,
        |_| true,
        |_| false,
        |_| false,
        |_| false,
        |_| RowLiveness::Unknown,
        true,
    );
    let ch = changes.iter().find(|c| c.name == "t-x30c2-w1").unwrap();
    assert_eq!(
        ch.new_liveness,
        Some("alive"),
        "the probe word is what gets served"
    );
    assert_eq!(
        ch.new_status,
        Some(AgentStatus::Live),
        "recovery still flips the stored status"
    );
}

#[test]
fn session_transition_apply_preserves_succession_and_splits_live_branch() {
    let mut registry = state::Registry::default();
    let mut predecessor = rentry("worker", AgentStatus::Live, None);
    predecessor.harness_session_id = Some("session-a".into());
    predecessor.fno_id = Some("thread-a".into());
    registry.entries.push(predecessor);

    assert_eq!(
        apply_session_transition(&mut registry, "worker", "session-b", Some(false), "", "",)
            .unwrap(),
        state::SessionTransition::Succession
    );
    assert_eq!(registry.entries.len(), 1);
    assert_eq!(registry.entries[0].fno_id.as_deref(), Some("thread-a"));
    assert_eq!(
        registry.entries[0].predecessor_session_ids,
        vec!["session-a"]
    );

    assert_eq!(
        apply_session_transition(
            &mut registry,
            "worker",
            "session-c",
            Some(true),
            "worker-branch",
            "thread-c",
        )
        .unwrap(),
        state::SessionTransition::Branch
    );
    assert_eq!(registry.entries.len(), 2);
    assert_eq!(
        registry.entries[0].harness_session_id.as_deref(),
        Some("session-b")
    );
    assert_eq!(
        registry.entries[1].harness_session_id.as_deref(),
        Some("session-c")
    );
    assert_eq!(
        registry.entries[1].forked_from_session_id.as_deref(),
        Some("session-b")
    );
    assert_eq!(registry.entries[1].fno_id.as_deref(), Some("thread-c"));
    assert_ne!(registry.entries[0].fno_id, registry.entries[1].fno_id);

    assert_eq!(
        apply_session_transition(
            &mut registry,
            "worker",
            "session-d",
            Some(true),
            "worker-branch",
            "thread-d",
        )
        .unwrap(),
        state::SessionTransition::Branch
    );
    let second_branch = registry
        .entries
        .iter()
        .find(|entry| entry.fno_id.as_deref() == Some("thread-d"))
        .expect("second branch row");
    assert_eq!(second_branch.name, "worker-branch-2");
}
