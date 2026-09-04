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
