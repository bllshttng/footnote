//! The dormant gate: when a row idle past grace leaves on a `done` tail.
//!
//! Split out of `daemon.rs`, which is over the shrink-only file budget. A
//! child of `daemon::tests` so it keeps the same fixtures (`tmp_home`,
//! `ask_row`, `tails_for`, `civil`) rather than growing a second set.

use super::*;

/// x-0d93: the dormant gate spends ONE subprocess per sweep, not one per
/// idle row, and no row is dropped by a cap.
///
/// `DORMANT_PROBE_CAP` used to stop the sweep at 8 probes, judging an
/// arbitrary first eight of a large roster and reporting nothing about the
/// rest - a silent truncation living in production. Batching removed the
/// reason it existed, so all 40 rows here are judged in one sweep and the
/// spend is reported on the summary.
#[test]
fn gc_dormant_gate_batches_every_idle_row_into_one_call_with_no_cap() {
    let home = tmp_home("gc-dormant-batch");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let idle_since = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    let rows = 40;
    state::update_registry(&home.registry_json(), |r| {
        for i in 0..rows {
            let mut row = ask_row(&format!("bg-idle-{i:02}"), None);
            row.status = AgentStatus::Live;
            row.short_id = format!("idle{i:02}");
            row.last_message_at = Some(idle_since.clone());
            row.pid = Some(std::process::id());
            r.entries.push(row);
        }
    })
    .unwrap();

    let calls = std::cell::RefCell::new(Vec::new());
    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &|handles: &[String]| {
            calls.borrow_mut().push(handles.to_vec());
            // Every row answers "working": alive, so none may be evicted.
            handles
                .iter()
                .map(|h| (h.clone(), "working".to_string()))
                .collect()
        },
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    let calls = calls.into_inner();
    assert_eq!(calls.len(), 1, "one sweep, one truth subprocess");
    assert_eq!(calls[0].len(), rows, "no row is dropped by a cap");
    assert_eq!(summary.dormant_probes_escalated, rows);
    assert!(
        summary.reaped_dormant.is_empty() && summary.reaped.is_empty(),
        "a working tail is not a done tail"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(reg.entries.len(), rows);
}

/// STALENESS MUST NEVER REAP, pinned. A row can be quiet for hours and
/// still be alive - a worker waiting on CI is the ordinary case.
///
/// Two ways the sweep could get this wrong, both asserted: a `working`
/// tail must keep the row, and a row the probe could not answer for at all
/// must keep it too. Only the probe's POSITIVE `done` reading evicts, and
/// an unanswered handle is an absence, never a death sentence.
#[test]
fn gc_dormant_gate_never_reaps_a_quiet_row_the_probe_did_not_call_done() {
    let home = tmp_home("gc-dormant-noreap");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 6 * 3600);
    let quiet_since = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        for (name, short) in [("bg-on-ci", "onci"), ("bg-mute", "mute")] {
            let mut row = ask_row(name, None);
            row.status = AgentStatus::Live;
            row.short_id = short.into();
            row.last_message_at = Some(quiet_since.clone());
            row.pid = Some(std::process::id());
            r.entries.push(row);
        }
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        // "onci" answers working. "mute" is absent from the map entirely:
        // the batch could not answer for it.
        &tails_for(|handle| (handle == "onci").then(|| "working".to_string())),
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert_eq!(summary.dormant_probes_escalated, 2, "both were asked");
    assert!(summary.reaped_dormant.is_empty());
    assert!(summary.reaped.is_empty());
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        reg.entries.iter().any(|e| e.name == "bg-on-ci"),
        "six hours of quiet with a working tail is alive, not gone"
    );
    assert!(
        reg.entries.iter().any(|e| e.name == "bg-mute"),
        "an unanswered probe is an absence; it must never evict"
    );
}

/// The outcome test, by name: a finished thread row LEAVES and a live row
/// STAYS in the same pass.
///
/// A thread row carries no pid by design, so a restarted daemon does not
/// host it, `is_live` reads false, its tail was never probed, and
/// `NotTerminal` held it forever - 28 of 58 rows on the machine this was
/// measured on, and all of the registry's growth. A run that removes
/// nothing is the behaviour this test exists to fail on, so both halves
/// are asserted BY NAME.
#[test]
fn gc_sweep_reaps_a_finished_pid_less_thread_row_and_keeps_the_live_one() {
    let home = tmp_home("gc-thread-dormant");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 86_400);
    let two_days_ago = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    state::update_registry(&home.registry_json(), |r| {
        let mut thread = ask_row("t-finished-thread", None);
        thread.status = AgentStatus::Live;
        thread.short_id = "thr1".into();
        thread.last_message_at = Some(two_days_ago.clone());
        thread.pid = None;
        // A real THREAD row, not a one-shot ask. The gate reads every tail
        // now, and a finished ask leaves on a `done` tail too (its own
        // criterion below); this fixture stays a thread row so the test still
        // names the population it was written for.
        thread.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        r.entries.push(thread);

        let mut live = ask_row("t-still-working", None);
        live.status = AgentStatus::Live;
        live.short_id = "live1".into();
        live.pid = Some(std::process::id());
        r.entries.push(live);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &tails_for(|handle| (handle == "thr1").then(|| "done".to_string())),
        &|_| None,
        &live_row_liveness,
        &|_| None,
    );

    assert!(
        summary
            .reaped_dormant
            .iter()
            .any(|id| id.contains("t-finished-thread") || id == "thr1"),
        "the finished thread row stayed: {:?}",
        summary.reaped_dormant
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        !reg.entries.iter().any(|e| e.name == "t-finished-thread"),
        "the row is still in the registry"
    );
    assert!(
        reg.entries.iter().any(|e| e.name == "t-still-working"),
        "the live row was taken with it"
    );
}

/// A live row whose transcript is ADVANCING costs no probe, and the reason
/// is `row_idle_secs`, not a stat gate.
///
/// x-0d93 planned a transcript-size gate in front of the dormant probe:
/// grown since the last sweep -> skip. This test is why that gate is not
/// here. The dormant gate only opens when `idle > grace`, and `idle` is
/// `now - max(last_message_at, transcript mtime)`. So the gate opening
/// ALREADY proves the transcript has not been touched for over an hour
/// (the default grace), while sweeps run every 5 seconds. A row whose
/// transcript grew since the last sweep has an mtime seconds old and never
/// reaches the probe at all.
///
/// The `stat_records` map is passed EMPTY here, which is the cold-sweep
/// input a size gate would have to escalate on. The row is still not
/// probed. Nothing a size comparison could add is reachable, and a guard
/// whose skip arm cannot fire is decoration.
#[test]
fn a_live_row_with_a_growing_transcript_is_never_probed_by_the_dormant_gate() {
    let home = tmp_home("gc-advancing");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcript = home.root().join("advancing.jsonl");
    std::fs::write(&transcript, b"{\"type\":\"assistant\"}\n").unwrap();

    // `last_message_at` two hours stale against a 1h grace: on that field
    // alone this row is long idle and the gate would open.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("bg-advancing", None);
        row.status = AgentStatus::Live;
        row.short_id = "bgadv".into();
        row.last_message_at = Some(format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"));
        row.pid = Some(std::process::id());
        r.entries.push(row);
    })
    .unwrap();

    let asked = std::cell::RefCell::new(Vec::new());
    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &|handles: &[String]| {
            asked.borrow_mut().extend(handles.iter().cloned());
            std::collections::HashMap::new()
        },
        // The transcript written a moment ago IS this row's newest match.
        &|_| Some(vec![transcript.clone()]),
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert!(
        asked.into_inner().is_empty(),
        "a fresh transcript mtime closes the dormant gate before any probe is considered"
    );
    assert_eq!(summary.dormant_probes_escalated, 0);
    assert!(summary.reaped_dormant.is_empty());
}

/// AC7 end-to-end: a LIVE row idle past grace whose tail reads done leaves
/// as a dormant reap (resumable: true in the event); one whose tail reads
/// anything else stays. The credential-dead worker is the second case.
#[test]
fn gc_sweep_live_done_row_leaves_as_dormant_and_other_tails_stay() {
    let home = tmp_home("gc-dormant");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now - 2 * 3600);
    let idle_since = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    // Live rows: our own pid, bare-existence live (the fixture pattern the
    // keeps_live test uses), idle two hours against a 1h grace.
    state::update_registry(&home.registry_json(), |r| {
        let mut done = ask_row("bg-done", None);
        done.status = AgentStatus::Live;
        done.short_id = "bgdone".into();
        done.last_message_at = Some(idle_since.clone());
        done.pid = Some(std::process::id());
        r.entries.push(done);
        let mut watching = ask_row("bg-watch", None);
        watching.status = AgentStatus::Live;
        watching.short_id = "bgwatch".into();
        watching.last_message_at = Some(idle_since.clone());
        watching.pid = Some(std::process::id());
        r.entries.push(watching);
    })
    .unwrap();

    let summary = gc_sweep_impl(
        &home,
        &emitter,
        &|_| Duration::from_secs(3600),
        false,
        7,
        &tails_for(|handle| match handle {
            "bgdone" => Some("done".to_string()),
            _ => Some("watching".to_string()), // the credential-dead shape
        }),
        &|_| None,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    );

    assert_eq!(summary.reaped_dormant, vec!["bgdone".to_string()]);
    assert!(summary.reaped.is_empty(), "a finished turn is not a death");
    let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
    assert!(
        events.contains("\"resumable\":true"),
        "the dormant reap must record resumability for the dormant distinction"
    );
    // POSITIVE MARKER: only the done row left the registry.
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().all(|e| e.name != "bg-done"));
    assert!(
        reg.entries.iter().any(|e| e.name == "bg-watch"),
        "a live row whose tail is not done stays, whatever its idle age"
    );
}

// -- one test per acceptance criterion --------------------------------------
//
// The fixtures mirror the shapes the 2026-09-04 measurement took of the live
// registry, by name: a codex thread row (`t-orphan-prs`), the codex exec row
// that is not a thread entry (`01a06844`), a claude bg row whose `log_path`
// is a 0-byte wrapper log (`d298725d`'s shape), the no-clock row
// (`2529b52b`), and the live row (`119e3c52`). Everything runs through the
// injected `truth_tail_states` / `store_matches` seams, so none of it
// touches a real harness store.

/// A pid-less row the ladder answers Unknown on: the shape the measured
/// registry's kept-not-terminal majority shares.
fn pid_less_row(name: &str, short_id: &str) -> RegistryEntry {
    let mut row = ask_row(name, None);
    row.status = AgentStatus::Live;
    row.short_id = short_id.into();
    row
}

/// An RFC-like stamp `hours_ago` in the past, the registry's own spelling.
fn hours_ago_stamp(hours_ago: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(now.saturating_sub(hours_ago * 3600));
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// One sweep over `home` with the staged tail answers and store matches;
/// every argument but the two seams is the production shape.
fn sweep(
    home: &AgentsHome,
    tails: &dyn Fn(&[String]) -> std::collections::HashMap<String, String>,
    store: &dyn Fn(&state::RegistryEntry) -> Option<Vec<std::path::PathBuf>>,
) -> GcSummary {
    gc_sweep_impl(
        home,
        &EventEmitter::new(home.events_jsonl(), "daemon"),
        &|_| Duration::from_secs(3600),
        false,
        7,
        tails,
        store,
        &live_row_liveness, // x-5d96: injectable so tests stage the ladder
        &|_| None,
    )
}

/// AC1: the positive REMOVAL marker, by name. A codex thread row shaped like
/// `t-7e0b-mint-2` - empty short_id, no pid, a real rollout on disk, idle
/// past grace - whose batched probe answers `done` for its NAME is escalated,
/// lands in `reaped_dormant` under that name, and leaves the registry.
#[test]
fn ac1_a_codex_thread_row_reading_done_is_escalated_and_removed_by_name() {
    let home = tmp_home("gc-ac1");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = pid_less_row("t-7e0b-mint-2", "");
        row.harness = Some("codex".into());
        row.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        row.last_message_at = Some(hours_ago_stamp(2));
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| (handle == "t-7e0b-mint-2").then(|| "done".to_string())),
        &|_| None,
    );

    assert_eq!(
        summary.dormant_probes_escalated, 1,
        "escalated under its name, not its (empty) short_id"
    );
    assert_eq!(
        summary.reaped_dormant,
        vec!["t-7e0b-mint-2".to_string()],
        "removed BY NAME"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(
        reg.entries.iter().all(|e| e.name != "t-7e0b-mint-2"),
        "gone from the registry"
    );
}

/// AC2: the positive PROBE-RAN marker, by name; staleness never reaps. A row
/// shaped like `t-orphan-prs` whose probe answers `stalled` is escalated,
/// kept, and reported with exactly `tail: stalled` - never a never-asked
/// verdict, and absent from every reap bucket.
#[test]
fn ac2_a_codex_thread_row_reading_stalled_is_probed_reported_and_kept() {
    let home = tmp_home("gc-ac2");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = pid_less_row("t-orphan-prs", "");
        row.harness = Some("codex".into());
        row.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        row.last_message_at = Some(hours_ago_stamp(2));
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| (handle == "t-orphan-prs").then(|| "stalled".to_string())),
        &|_| None,
    );

    assert_eq!(summary.dormant_probes_escalated, 1, "the probe ran");
    assert!(
        summary.reaped.is_empty()
            && summary.reaped_dormant.is_empty()
            && summary.reaped_backstop.is_empty(),
        "stalled is an absence; only `done` evicts"
    );
    assert!(
        summary
            .kept_not_terminal
            .iter()
            .any(|(id, reason)| id == "t-orphan-prs" && reason == "tail: stalled"),
        "exactly the tail verdict, by name: {:?}",
        summary.kept_not_terminal
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.name == "t-orphan-prs"));
}

/// AC3: the exec-mode codex row (`01a06844`'s shape). Empty short_id and
/// `host_mode: exec`, so `is_codex_thread_entry` is false - a fix that only
/// admits interactive thread rows would leave it unprobed. It gets its tail
/// read and a `tail: ...` verdict.
#[test]
fn ac3_a_codex_exec_row_with_no_short_id_gets_its_tail_read() {
    let home = tmp_home("gc-ac3");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = pid_less_row("01a06844", "");
        row.harness = Some("codex".into());
        row.host_mode = Some(crate::state::HOST_MODE_EXEC.to_string());
        row.last_message_at = Some(hours_ago_stamp(2));
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| (handle == "01a06844").then(|| "stalled".to_string())),
        &|_| None,
    );

    assert_eq!(
        summary.dormant_probes_escalated, 1,
        "admitted by pid-lessness, not by thread identity"
    );
    assert!(
        summary
            .kept_not_terminal
            .iter()
            .any(|(id, reason)| id == "01a06844" && reason == "tail: stalled"),
        "reported with the tail verdict, by name: {:?}",
        summary.kept_not_terminal
    );
}

/// AC4: the claude bg majority. A row shaped like `d298725d` - non-empty
/// short_id, no pid, `host_mode: exec`, a real transcript in its own store,
/// and a 0-byte wrapper `log_path` - reads its idle clock from the STORE
/// transcript, not the wrapper log. The store transcript is the row's ONLY
/// activity signal (`last_message_at` is `None`), and it is stamped two hours
/// old while the wrapper log is written fresh by the test: escalation at all
/// proves the store won, because the wrapper log's spawn-time mtime would
/// have held the gate shut.
#[test]
fn ac4_the_idle_clock_of_a_claude_bg_row_comes_from_its_own_store_transcript() {
    let home = tmp_home("gc-ac4");
    let wrapper = home.root().join("wrapper.log");
    std::fs::write(&wrapper, b"").unwrap();
    let store_transcript = home.root().join("store.jsonl");
    std::fs::write(&store_transcript, b"{\"type\":\"assistant\"}\n").unwrap();
    let two_hours_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 2 * 3600;
    std::fs::File::options()
        .write(true)
        .open(&store_transcript)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(two_hours_ago),
        ))
        .unwrap();

    state::update_registry(&home.registry_json(), |r| {
        let mut row = pid_less_row("bg-d298725d", "d298725d");
        row.host_mode = Some(crate::state::HOST_MODE_EXEC.to_string());
        row.harness_session_id = Some("d298725d-sess".into());
        row.log_path = Some(wrapper.to_string_lossy().into_owned());
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| (handle == "d298725d").then(|| "working".to_string())),
        &|e: &state::RegistryEntry| {
            (e.short_id == "d298725d").then(|| vec![store_transcript.clone()])
        },
    );

    assert_eq!(
        summary.dormant_probes_escalated, 1,
        "escalated on the store clock; the wrapper log would have held the gate shut"
    );
    assert!(
        summary
            .kept_not_terminal
            .iter()
            .any(|(id, reason)| id == "d298725d" && reason == "tail: working"),
        "reported with the tail verdict: {:?}",
        summary.kept_not_terminal
    );
}

/// AC5: the named behavior delta. A finished one-shot claude ask whose probe
/// answers `done` now reaches `reaped_dormant` - it was excluded before, and
/// that is intended: it is a finished ask, and reconcile already settles it.
/// The test exists so the delta is never mistaken for a regression.
#[test]
fn ac5_a_finished_one_shot_claude_ask_reading_done_reaches_reaped_dormant() {
    let home = tmp_home("gc-ac5");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("bg-ask-finished", None);
        row.status = AgentStatus::Live;
        row.last_message_at = Some(hours_ago_stamp(2));
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| (handle == "bg-ask-finished").then(|| "done".to_string())),
        &|_| None,
    );

    assert_eq!(
        summary.dormant_probes_escalated, 1,
        "the gate opened for a one-shot ask - the intended delta"
    );
    assert_eq!(
        summary.reaped_dormant,
        vec!["bg-ask-finished".to_string()],
        "a finished ask leaves exactly as a finished thread does"
    );
}

/// AC6: an absence never evicts, three ways. Three rows past grace whose
/// probe answers `stalled`, `unknown`, and nothing at all (the handle absent
/// from the batch map): none is reaped, and their reasons read the tail
/// verdicts the arm actually took.
#[test]
fn ac6_an_absence_never_evicts_three_ways() {
    let home = tmp_home("gc-ac6");
    state::update_registry(&home.registry_json(), |r| {
        for (name, short) in [
            ("bg-stalled", "st1"),
            ("bg-unknown", "un1"),
            ("bg-missing", "ab1"),
        ] {
            let mut row = pid_less_row(name, short);
            row.last_message_at = Some(hours_ago_stamp(2));
            r.entries.push(row);
        }
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| match handle {
            "st1" => Some("stalled".to_string()),
            "un1" => Some("unknown".to_string()),
            _ => None,
        }),
        &|_| None,
    );

    assert_eq!(summary.dormant_probes_escalated, 3, "all three were asked");
    assert!(
        summary.reaped.is_empty()
            && summary.reaped_dormant.is_empty()
            && summary.reaped_backstop.is_empty(),
        "no probe outcome but a literal `done` may evict"
    );
    for (id, reason) in [
        ("st1", "tail: stalled"),
        ("un1", "tail: unknown"),
        ("ab1", "tail: unknown"),
    ] {
        assert!(
            summary
                .kept_not_terminal
                .iter()
                .any(|(kept, r)| kept == id && r == reason),
            "row {id} kept with reason {reason:?}: {:?}",
            summary.kept_not_terminal
        );
    }
    let reg = state::load_registry(&home.registry_json()).unwrap();
    for (name, _) in [("bg-stalled", ""), ("bg-unknown", ""), ("bg-missing", "")] {
        assert!(
            reg.entries.iter().any(|e| e.name == name),
            "{name} was taken by an absence"
        );
    }
}

/// AC7: the no-clock row survives and says so. Shaped like `2529b52b` - no
/// harness session id, no `last_message_at`, a 0-byte wrapper `log_path`,
/// not terminal - it is never probed, is kept, and its reason is exactly
/// `not probed: no idle signal`. Idleness cannot be proven for it, so
/// nothing may be inferred.
#[test]
fn ac7_the_no_clock_row_survives_and_names_its_gate() {
    let home = tmp_home("gc-ac7");
    let wrapper = home.root().join("wrapper7.log");
    std::fs::write(&wrapper, b"").unwrap();
    state::update_registry(&home.registry_json(), |r| {
        let mut row = pid_less_row("bg-2529b52b", "2529b52b");
        row.harness_session_id = None;
        row.log_path = Some(wrapper.to_string_lossy().into_owned());
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(&home, &tails_for(|_| None), &|_| None);

    assert_eq!(
        summary.dormant_probes_escalated, 0,
        "no idle signal closes the gate before any probe is considered"
    );
    assert!(
        summary
            .kept_not_terminal
            .iter()
            .any(|(id, reason)| id == "2529b52b" && reason == "not probed: no idle signal"),
        "the gate that held it is named, under its handle: {:?}",
        summary.kept_not_terminal
    );
    assert!(
        summary.reaped.is_empty()
            && summary.reaped_dormant.is_empty()
            && summary.reaped_backstop.is_empty()
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.name == "bg-2529b52b"));
}

/// AC8: a live row survives, by name. The row the ladder reports live (the
/// `119e3c52` case in the inherited measurement) lands in `kept_live` and in
/// no reap bucket. This plan must not move a single live row.
#[test]
fn ac8_a_live_row_survives_by_name() {
    let home = tmp_home("gc-ac8");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = pid_less_row("t-119e3c52-live", "119e3c52");
        row.pid = Some(std::process::id());
        row.last_message_at = Some(hours_ago_stamp(2));
        r.entries.push(row);
    })
    .unwrap();

    let summary = sweep(
        &home,
        &tails_for(|handle| (handle == "119e3c52").then(|| "working".to_string())),
        &|_| None,
    );

    assert!(
        summary.kept_live.iter().any(|id| id == "119e3c52"),
        "live and kept: {:?}",
        summary.kept_live
    );
    assert!(
        summary.reaped.is_empty()
            && summary.reaped_dormant.is_empty()
            && summary.reaped_backstop.is_empty(),
        "no reap bucket carries a live row"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.short_id == "119e3c52"));
}

/// AC9: the retired never-asked verdict is gone from the code that used to
/// print it, pinned here so a future edit re-introducing it fails a test
/// instead of a grep nobody remembers to run. The renderer's own test pins
/// the two new spellings; this asserts they are the pinned ones.
#[test]
fn ac9_the_retired_never_asked_verdict_is_gone_from_its_sources() {
    for src in [
        include_str!("../../daemon.rs"),
        include_str!("../../gc.rs"),
        include_str!("../../reap_render.rs"),
    ] {
        assert!(
            !src.contains("no tail read"),
            "the retired verdict string is back in a source file"
        );
    }
    assert!(
        include_str!("../../reap_render.rs").contains("not probed: no idle signal"),
        "the renderer test must pin the two new spellings"
    );
}
