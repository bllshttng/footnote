use super::*;

#[test]
fn printed_numbers_and_row_come_from_one_dict() {
    let mut readings = sample_readings(board7(), court4(), cap_ok(), workers3());
    set_reading(
        &mut readings,
        Reading::took(
            "workers",
            json!({
                "live_workers": 3,
                "oldest_worker_seen": "90s w1",
                "live_subagents": null,
            }),
        ),
    );
    set_reading(
        &mut readings,
        Reading::took(
            "watch_expiry",
            json!({"rows": [{"session_id": "s-late", "overdue_ms": 125_000}]}),
        ),
    );
    let data = build_data(&readings, "x-bbbb");
    let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
    let board_line = lines.iter().find(|l| l.starts_with("board:")).unwrap();
    assert!(board_line.contains("open_prs 7"), "line: {board_line}");
    assert!(board_line.contains("blocked 2"));
    let workers_line = lines.iter().find(|l| l.starts_with("workers:")).unwrap();
    assert!(workers_line.contains("live 3"));
    assert!(
        workers_line.contains("overdue s-late 125s"),
        "line: {workers_line}"
    );
    assert_eq!(data.get("coverage"), Some(&json!(22)));
    assert_eq!(data.get("open_prs"), Some(&json!(7)));
    assert!(lines.iter().any(|l| l == "coverage: 22 of 22 readings ok"));
}

#[test]
fn failed_reader_prints_own_line_and_beat_continues() {
    let mut readings = sample_readings(Value::Null, court4(), cap_ok(), workers3());
    set_reading(
        &mut readings,
        Reading::took(
            "workers",
            json!({
                "live_workers": 3,
                "oldest_worker_seen": "90s w1",
                "live_subagents": null,
            }),
        ),
    );
    set_reading(
        &mut readings,
        Reading::failed(
            "watch_expiry",
            "watch expiry event has no valid timestamp".into(),
        ),
    );
    set_reading(
        &mut readings,
        Reading::failed("board", "board payload names no undriven_pr queue".into()),
    );
    let data = build_data(&readings, "x-bbbb");
    let change = derive_change(None, &data, "");
    let lines = render_lines("x-bbbb", &readings, &data, &None, "", &change);
    assert!(lines.iter().any(|l| l.starts_with("READER FAILED board:")));
    assert!(lines
        .iter()
        .any(|l| l == "READER FAILED watch expiry: watch expiry event has no valid timestamp"));
    assert!(lines
        .iter()
        .any(|l| l.starts_with("workers:") && l.contains("overdue watches unmeasured")));
    assert!(lines
        .iter()
        .any(|l| l.starts_with("coverage: 20 of 22 readings ok")));
    assert!(lines.iter().any(|l| l.contains("failed readers: board (")
        && l.contains(", watch_expiry (watch expiry event has no valid timestamp)")));
    assert_eq!(
        change,
        "no numeric movement; readings failed: board, watch_expiry"
    );
    assert_eq!(data.get("open_prs"), None);
    assert_eq!(
        data.get("readers_failed"),
        Some(&json!(["board", "watch_expiry"]))
    );
}
