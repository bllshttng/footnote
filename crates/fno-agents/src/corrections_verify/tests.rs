use super::*;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("corr-verify-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A termination row in the envelope shape append_loop_event writes:
/// `{"ts", "type", "source", "data"}`.
fn termination(id: &str, ts: &str, reason: &str) -> String {
    format!(
        r#"{{"ts":"{ts}","type":"termination","source":"hook","data":{{"session_id":"{id}","reason":"{reason}","message":""}}}}"#
    )
}

fn block(id: &str, ts: &str) -> String {
    format!(
        r#"{{"ts":"{ts}","type":"loop_check","source":"hook","data":{{"session_id":"{id}","decision":"block"}}}}"#
    )
}

fn run_json(log: &std::path::Path, events: &std::path::Path, now: &str) -> Vec<Value> {
    let flags = |out: &mut Vec<String>| {
        out.push("--json".into());
        out.push("--log".into());
        out.push(log.to_string_lossy().into_owned());
        out.push("--events".into());
        out.push(events.to_string_lossy().into_owned());
        out.push("--now".into());
        out.push(now.into());
    };
    let mut argv = Vec::new();
    flags(&mut argv);
    assert_eq!(run(&argv), 0);
    // The JSON arrives on process stdout; read it through the file the
    // runner would see by re-running score() under the same inputs instead
    // of spawning a process.
    let verdicts = score(
        &read_corrections(
            &std::fs::read_to_string(log).unwrap(),
            parse_ts("2026-01-01T00:00:00Z").unwrap(),
        ),
        &read_sessions(&std::fs::read_to_string(events).unwrap()),
    );
    json_rows(&verdicts).as_array().unwrap().clone()
}

#[test]
fn improved_when_friction_falls_across_the_correction() {
    let dir = temp_dir("improved");
    let log = dir.join("corrections.log");
    let events = dir.join("events.jsonl");
    // One applied correction between the two session clusters.
    std::fs::write(
        &log,
        concat!(
            "2026-09-10T12:00:00Z | S1 | git-rule-edit | rules/style.md | enforce emdash ban\n",
            "2026-09-10T12:00:00Z | S1 | target-postmortem | /tmp/pm.md | NoProgress: d\n"
        ),
    )
    .unwrap();
    // 3 stuck sessions before (10-01..10-03), 3 clean after (10-20..10-22).
    std::fs::write(
        &events,
        [
            termination("s1", "2026-09-01T10:00:00Z", "NoProgress"),
            termination("s2", "2026-09-02T10:00:00Z", "Budget"),
            termination("s3", "2026-09-03T10:00:00Z", "Aborted"),
            termination("s4", "2026-09-20T10:00:00Z", "DonePRGreen"),
            termination("s5", "2026-09-21T10:00:00Z", "DonePRGreen"),
            termination("s6", "2026-09-22T10:00:00Z", "DonePRGreen"),
        ]
        .join("\n"),
    )
    .unwrap();
    let verdicts = score(
        &read_corrections(
            &std::fs::read_to_string(&log).unwrap(),
            parse_ts("2026-01-01T00:00:00Z").unwrap(),
        ),
        &read_sessions(&std::fs::read_to_string(&events).unwrap()),
    );
    assert_eq!(verdicts.len(), 1, "the postmortem row is never scored");
    let v = &verdicts[0];
    assert_eq!(v.verdict, "improved", "{v:?}");
    assert_eq!((v.sessions_before, v.sessions_after), (3, 3), "{v:?}");
    assert_eq!(v.before, 1.0);
    assert_eq!(v.after, 0.0);
    assert_eq!(v.ratio, Some(0.0));
    assert!(v.ratio.unwrap() < 0.7);
    // The --json row carries the same facts.
    let rows = run_json(&log, &events, "2026-09-25T00:00:00Z");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["verdict"], json!("improved"));
    assert_eq!(rows[0]["file"], json!("rules/style.md"));
    assert_eq!(rows[0]["sessions_before"], json!(3));
    assert_eq!(rows[0]["sessions_after"], json!(3));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn too_few_sessions_after_reads_insufficient_data() {
    let dir = temp_dir("insufficient");
    let log = dir.join("corrections.log");
    let events = dir.join("events.jsonl");
    std::fs::write(
        &log,
        "2026-09-10T12:00:00Z | S1 | git-rule-edit | rules/style.md | enforce emdash ban\n",
    )
    .unwrap();
    std::fs::write(
        &events,
        [
            termination("s1", "2026-09-01T10:00:00Z", "NoProgress"),
            termination("s2", "2026-09-02T10:00:00Z", "Budget"),
            termination("s3", "2026-09-03T10:00:00Z", "Aborted"),
            termination("s4", "2026-09-20T10:00:00Z", "DonePRGreen"),
            termination("s5", "2026-09-21T10:00:00Z", "DonePRGreen"),
        ]
        .join("\n"),
    )
    .unwrap();
    let verdicts = score(
        &read_corrections(
            &std::fs::read_to_string(&log).unwrap(),
            parse_ts("2026-01-01T00:00:00Z").unwrap(),
        ),
        &read_sessions(&std::fs::read_to_string(&events).unwrap()),
    );
    let v = &verdicts[0];
    assert_eq!(v.verdict, "insufficient-data", "{v:?}");
    assert_eq!((v.sessions_before, v.sessions_after), (3, 2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn blocks_count_into_friction_and_window_bounds_hold() {
    let dir = temp_dir("bounds");
    let log = dir.join("corrections.log");
    let events = dir.join("events.jsonl");
    std::fs::write(
        &log,
        "2026-09-15T12:00:00Z | S1 | git-rule-edit | rules/loop.md | stop polling\n",
    )
    .unwrap();
    std::fs::write(
        &events,
        [
            // before: 3 clean ends, one carrying 3 blocks of loop friction
            termination("s1", "2026-09-01T10:00:00Z", "DonePRGreen"),
            block("s1", "2026-09-01T09:00:00Z"),
            block("s1", "2026-09-01T09:01:00Z"),
            block("s1", "2026-09-01T09:02:00Z"),
            termination("s6", "2026-09-02T10:00:00Z", "DonePRGreen"),
            termination("s7", "2026-09-03T10:00:00Z", "DonePRGreen"),
            // at the boundary ts: neither window counts it
            termination("s2", "2026-09-15T12:00:00Z", "DonePRGreen"),
            // after: clean
            termination("s3", "2026-09-16T10:00:00Z", "DonePRGreen"),
            termination("s4", "2026-09-17T10:00:00Z", "DonePRGreen"),
            termination("s5", "2026-09-18T10:00:00Z", "DonePRGreen"),
        ]
        .join("\n"),
    )
    .unwrap();
    let sessions = read_sessions(&std::fs::read_to_string(&events).unwrap());
    let verdicts = score(
        &read_corrections(
            &std::fs::read_to_string(&log).unwrap(),
            parse_ts("2026-01-01T00:00:00Z").unwrap(),
        ),
        &sessions,
    );
    let v = &verdicts[0];
    // s2 ends exactly at the correction ts: excluded from both windows.
    assert_eq!((v.sessions_before, v.sessions_after), (3, 3), "{v:?}");
    assert_eq!(v.before, 1.0);
    assert_eq!(v.after, 0.0);
    assert_eq!(v.verdict, "improved");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn markdown_maps_verdicts_to_verbs() {
    let mk = |verdict: &'static str| Verdict {
        ts: parse_ts("2026-09-10T12:00:00Z").unwrap(),
        file: "rules/style.md".into(),
        details: "d".into(),
        sha: None,
        reference: None,
        before: 1.0,
        after: 0.5,
        ratio: Some(0.5),
        sessions_before: 3,
        sessions_after: 3,
        verdict,
    };
    let text = markdown(&[mk("improved"), mk("flat"), mk("insufficient-data")]);
    assert!(text.contains("improved (keep)"), "{text}");
    assert!(text.contains("flat (improve)"), "{text}");
    assert!(
        text.contains("insufficient-data (before=3, after=3)"),
        "{text}"
    );
    assert_eq!(markdown(&[]), "no applied corrections in window\n");
}

#[test]
fn skill_commit_rows_score_and_carry_their_link() {
    let log = concat!(
        "2026-09-10T12:00:00Z | S1 | skill-commit | skills/target/SKILL.md | autocorrect r1#1: fix skill sha=abc123def456 ref=r1#1\n",
        "2026-09-10T12:00:00Z | S1 | git-rule-edit | rules/style.md | enforce emdash ban\n",
    );
    let rows = read_corrections(log, parse_ts("2026-01-01T00:00:00Z").unwrap());
    assert_eq!(rows.len(), 2, "skill-commit scores beside git-rule-edit");
    assert_eq!(rows[0].sha.as_deref(), Some("abc123def456"));
    assert_eq!(rows[0].reference.as_deref(), Some("r1#1"));
    assert_eq!(rows[1].sha, None, "no tokens reads null");
    assert_eq!(rows[1].reference, None);
    // The --json row carries both fields; the markdown line names the ref.
    let verdicts = score(&rows, &[]);
    let json = json_rows(&verdicts);
    assert_eq!(json[0]["sha"], json!("abc123def456"));
    assert_eq!(json[0]["ref"], json!("r1#1"));
    assert_eq!(json[1]["sha"], serde_json::Value::Null);
    assert_eq!(json[1]["ref"], serde_json::Value::Null);
    let mut worse = verdicts[0].clone();
    worse.verdict = "worse";
    let text = markdown(std::slice::from_ref(&worse));
    assert!(
        text.contains("git revert abc123def456"),
        "the parsed sha drives the roll-back verb: {text}"
    );
    assert!(text.contains("ref=r1#1"), "{text}");
}

#[test]
fn a_subject_embedded_token_does_not_shadow_the_hook_token() {
    let log = "2026-09-10T12:00:00Z | S1 | skill-commit | skills/target/SKILL.md | see sha=deadbeefdead docs sha=abc123def456 ref=r1#1\n";
    let rows = read_corrections(log, parse_ts("2026-01-01T00:00:00Z").unwrap());
    assert_eq!(
        rows[0].sha.as_deref(),
        Some("abc123def456"),
        "the hook appends last, so its sha wins: {:?}",
        rows[0].sha
    );
    assert_eq!(rows[0].reference.as_deref(), Some("r1#1"));
}

#[test]
fn since_bounds_the_corrections_considered() {
    let log = concat!(
        "2026-09-01T12:00:00Z | S1 | git-rule-edit | rules/old.md | old\n",
        "2026-09-19T12:00:00Z | S1 | git-rule-edit | rules/new.md | new\n",
    );
    let since = parse_ts("2026-09-10T00:00:00Z").unwrap();
    let rows = read_corrections(log, since);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].file, "rules/new.md");
}

#[test]
fn windows_select_by_ts_not_by_session_id() {
    // Session ids that sort OPPOSITE to their timestamps: if the ends map
    // leaks its id order into the window slice, the wrong sessions land in
    // the before/after windows.
    let dir = temp_dir("tsorder");
    let log = dir.join("corrections.log");
    let events = dir.join("events.jsonl");
    std::fs::write(
        &log,
        "2026-09-10T12:00:00Z | S1 | git-rule-edit | rules/order.md | ts order\n",
    )
    .unwrap();
    std::fs::write(
        &events,
        [
            // ts order: z first (stuck), then a, b, c clean, then the
            // correction, then clean d, e, f and stuck g LAST.
            termination("z9", "2026-09-01T10:00:00Z", "NoProgress"),
            termination("a1", "2026-09-02T10:00:00Z", "DonePRGreen"),
            termination("b2", "2026-09-03T10:00:00Z", "DonePRGreen"),
            termination("c3", "2026-09-04T10:00:00Z", "DonePRGreen"),
            termination("d4", "2026-09-20T10:00:00Z", "DonePRGreen"),
            termination("e5", "2026-09-21T10:00:00Z", "DonePRGreen"),
            termination("f6", "2026-09-22T10:00:00Z", "DonePRGreen"),
            termination("g7", "2026-09-23T10:00:00Z", "Budget"),
        ]
        .join("\n"),
    )
    .unwrap();
    let verdicts = score(
        &read_corrections(
            &std::fs::read_to_string(&log).unwrap(),
            parse_ts("2026-01-01T00:00:00Z").unwrap(),
        ),
        &read_sessions(&std::fs::read_to_string(&events).unwrap()),
    );
    let v = &verdicts[0];
    // before = {a1, b2, c3} (0.0), NOT {z9, a1, b2} id-sorted; after =
    // {d4, e5, f6} (0.0), NOT the id-first {d4, e5, f6}... either way after
    // is clean; before must read 0.0 only if z9 (stuck, id-last) is
    // excluded by ts.
    assert_eq!((v.sessions_before, v.sessions_after), (3, 3), "{v:?}");
    assert_eq!(v.before, 0.0, "z9 (09-01) must not sit in the window head");
    assert_eq!(v.after, 0.0);
    // ratio 1.0 on two clean windows is flat, never improved: the stuck z9
    // session leaking into `before` would fake an improvement.
    assert_eq!(v.verdict, "flat", "{v:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_files_score_nothing_and_exit_zero() {
    let dir = temp_dir("missing");
    let code = run(&[
        "--json".into(),
        "--log".into(),
        dir.join("absent.log").to_string_lossy().into_owned(),
        "--events".into(),
        dir.join("absent-events.jsonl")
            .to_string_lossy()
            .into_owned(),
        "--now".into(),
        "2026-09-20T00:00:00Z".into(),
    ]);
    assert_eq!(code, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
