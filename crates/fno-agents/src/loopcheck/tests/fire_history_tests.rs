use super::*;

#[test]
fn target_stream_emit_lands_beside_legacy_lock_dirs() {
    // The store commit owns serialization now; a legacy lock or
    // maintenance marker beside the journal neither blocks nor drops a
    // hook emission.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("events.jsonl");
    let global = dir.path().join("global-events.jsonl");
    std::fs::create_dir(dir.path().join("events.jsonl.lock.d")).unwrap();
    std::fs::create_dir(dir.path().join("events.jsonl.gc.d")).unwrap();

    emit_to_both(&project, &global, "mutex_probe", serde_json::json!({}));

    for path in [&project, &global] {
        let text = crate::events::committed_journal_text(path);
        assert!(text.contains("mutex_probe"), "missing in {path:?}");
    }
}

#[test]
fn shadow_transition_accepts_without_emitting_a_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let run_log = dir.path().join("run-log.jsonl");
    let events = dir.path().join("events.jsonl");
    let run_id = "20260823T060900Z-cx73523-e04109";

    observe_shadow_transition(
        &run_log,
        run_id,
        crate::run_state::RunEvent::DispatchClassified,
        &events,
        &events,
    );

    assert_eq!(
        crate::run_state::fold_run_state(&run_log, run_id).unwrap(),
        crate::run_state::RunState::Working
    );
    assert!(!events.exists());
}

#[test]
fn shadow_transition_rejection_changes_no_legacy_decision() {
    let dir = tempfile::tempdir().unwrap();
    let run_log = dir.path().join("run-log.jsonl");
    let events = dir.path().join("events.jsonl");
    let run_id = "20260823T060900Z-cx73523-e04109";
    crate::run_state::append_transition(
        &run_log,
        run_id,
        crate::run_state::RunEvent::DispatchClassified,
    )
    .unwrap();
    crate::run_state::append_transition(
        &run_log,
        run_id,
        crate::run_state::RunEvent::PrepareHandoff,
    )
    .unwrap();
    crate::run_state::append_transition(
        &run_log,
        run_id,
        crate::run_state::RunEvent::SuccessorProven,
    )
    .unwrap();
    let legacy = allow_output("block", None, "keep working", 2, None);

    observe_shadow_transition(
        &run_log,
        run_id,
        crate::run_state::RunEvent::DispatchClassified,
        &events,
        &events,
    );

    assert_eq!(legacy, allow_output("block", None, "keep working", 2, None));
    let telemetry = crate::events::committed_journal_text(&events);
    assert!(telemetry.contains("\"type\":\"transition_rejected\""));
    assert!(telemetry.contains("invalid transition Closed + DispatchClassified"));
}

#[test]
fn shadow_observer_rejects_short_run_ids() {
    let dir = tempfile::tempdir().unwrap();
    let run_log = dir.path().join("run-log.jsonl");
    let events = dir.path().join("events.jsonl");

    observe_shadow_transition(
        &run_log,
        "short-run",
        crate::run_state::RunEvent::DispatchClassified,
        &events,
        &events,
    );

    assert!(!run_log.exists());
    assert!(crate::events::committed_journal_text(&events)
        .contains("manifest carries no valid full run id"));
}

#[test]
fn target_stream_emit_lands_during_legacy_maintenance_markers() {
    // The store commit is the acknowledgement; a maintenance marker
    // beside the journal retires no emission.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("events.jsonl");
    std::fs::create_dir(dir.path().join("events.jsonl.lock.d")).unwrap();
    std::fs::create_dir(dir.path().join("events.jsonl.gc.d")).unwrap();

    append_loop_event(&project, "review_coverage", serde_json::json!({}));

    assert!(
        crate::events::committed_journal_text(&project).contains("review_coverage"),
        "review coverage was dropped during expected maintenance"
    );
}

#[test]
fn target_stream_emit_lands_when_legacy_markers_clear_mid_flight() {
    // Markers created and removed around the emission: the store commit
    // is the acknowledgement boundary, so the row lands regardless.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("events.jsonl");
    let lock = dir.path().join("events.jsonl.lock.d");
    let maintenance = dir.path().join("events.jsonl.gc.d");
    std::fs::create_dir(&lock).unwrap();
    std::fs::create_dir(&maintenance).unwrap();

    append_loop_event(&project, "maintenance_handoff_probe", serde_json::json!({}));

    std::fs::remove_dir_all(maintenance).unwrap();
    std::fs::remove_dir_all(lock).unwrap();
    assert!(
        crate::events::committed_journal_text(&project).contains("maintenance_handoff_probe"),
        "the probe row was dropped"
    );
}

// ── streak debounce ─────────────────────────────────────────────
//
// These drive `read_prior_fires` with an explicit `now` and gap, so they need
// no env var and are parallel-safe -- unlike the integration suite, which
// pins FNO_LOOPCHECK_MIN_FIRE_GAP_SECS=0 process-wide.

const FP: &str = "FP";
const NOW: &str = "2026-06-05T12:00:00Z";

fn at(ts: &str) -> DateTime<Utc> {
    ts.parse().unwrap()
}

/// Write a loop_check events log from (ts, fingerprint) pairs, oldest first.
fn write_fire_log(path: &Path, fires: &[(String, &str)]) {
    let mut out = String::new();
    for (ts, fp) in fires {
        out.push_str(
            &serde_json::json!({
                "ts": ts, "type": "loop_check", "source": "hook",
                "data": { "session_id": "sess", "fingerprint": fp },
            })
            .to_string(),
        );
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
}

/// Count the streak over prior fires given as SECONDS BEFORE `now`, oldest
/// first, all sharing FP. Returns (streak, streak_window_secs).
fn streak_ago(secs_before_now: &[i64], gap: i64) -> (u64, i64) {
    let now = at(NOW);
    let fires: Vec<(String, &str)> = secs_before_now
        .iter()
        .map(|s| {
            (
                (now - chrono::Duration::seconds(*s))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
                FP,
            )
        })
        .collect();
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("events.jsonl");
    write_fire_log(&p, &fires);
    let (_, streak, _, window) = read_prior_fires(&p, "sess", Some(FP), now, gap);
    (streak, window)
}

/// The streak rules. `consecutive_after` is streak + 1, so a streak of 4 is
/// what trips the attended backstop of 5.
#[test]
fn debounce_streak_counting_rules() {
    // (case, prior fires as seconds before now (oldest first), gap, streak, window)
    #[rustfmt::skip]
        let cases: &[(&str, &[i64], i64, u64, i64)] = &[
            // AC1-HP: the triggering shape - four fires inside 60s are ONE
            // observation (the current fire), nowhere near backstop_n.
            ("rapid burst collapses to one observation", &[49, 33, 16, 0], 300, 0, 0),
            // AC2-HP: a genuinely stalled session is still reaped.
            ("fires 6 minutes apart still trip the backstop", &[1440, 1080, 720, 360], 300, 4, 1440),
            // AC3-FR: a skip must NOT advance the cursor. This fire is 330s
            // before `now` but only 270s before the burst's oldest member, so it
            // counts ONLY because the burst left the cursor parked at `now`.
            ("a skip does not advance the cursor", &[330, 60, 30, 10], 300, 1, 330),
            // AC6-FR: gap 0 is byte-identical to the old fire counting, which is
            // what lets the integration suite pin the seam and keep every
            // backstop assertion it already had.
            ("gap 0 restores fire counting exactly", &[49, 33, 16], 0, 3, 49),
            // Clock skew must not invent a debounce from a bad clock.
            ("a fire stamped after `now` counts, not crashes", &[1200, -600], 300, 2, 1200),
            // AC8-REG: the recorded sequence behind the false terminal - session
            // 20260727T203203Z, five fires in 109 seconds with CI still PENDING.
            ("the false-NoProgress incident now blocks", &[109, 93, 76, 17], 300, 0, 0),
        ];
    for (case, fires, gap, want_streak, want_window) in cases {
        let (streak, window) = streak_ago(fires, *gap);
        assert_eq!(streak, *want_streak, "streak: {case}");
        assert_eq!(window, *want_window, "window: {case}");
    }
}

/// AC4-CON: progress is never debounced - a CHANGED fingerprint breaks the
/// streak however fast it arrived.
#[test]
fn debounce_changed_fingerprint_breaks_streak_at_any_speed() {
    let now = at(NOW);
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("events.jsonl");
    write_fire_log(
        &p,
        &[
            ("2026-06-05T11:40:00Z".to_string(), FP),
            ("2026-06-05T11:50:00Z".to_string(), FP),
            ("2026-06-05T11:59:58Z".to_string(), "DIFFERENT"),
        ],
    );
    let (_, streak, _, _) = read_prior_fires(&p, "sess", Some(FP), now, 300);
    assert_eq!(streak, 0, "a 2-second-old change still resets the streak");
}

/// AC5-ERR: a fire we cannot place in time is transparent - it neither counts
/// toward nor breaks the streak, and never panics. Failing this way biases
/// away from an irreversible NoProgress.
#[test]
fn debounce_untimestamped_fire_is_transparent() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("events.jsonl");
    let lines = [
        r#"{"ts":"2026-06-05T11:40:00Z","type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
        r#"{"ts":"not-a-timestamp","type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
        r#"{"type":"loop_check","source":"hook","data":{"session_id":"sess","fingerprint":"FP"}}"#,
    ];
    std::fs::write(&p, lines.join("\n") + "\n").unwrap();

    let (_, streak, last_fp, _) = read_prior_fires(&p, "sess", Some(FP), at(NOW), 300);
    assert_eq!(
        streak, 1,
        "unplaceable fires skip; the good one still counts"
    );
    assert_eq!(
        last_fp.as_deref(),
        Some(FP),
        "carry-forward still reads the newest recorded fp"
    );
}

#[test]
fn fingerprint_format() {
    let fp = make_fingerprint("sha123", "OPEN", "SUCCESS", "2026-06-05T01:00:00Z");
    assert_eq!(fp, "sha123|OPEN|SUCCESS|2026-06-05T01:00:00Z");
}
