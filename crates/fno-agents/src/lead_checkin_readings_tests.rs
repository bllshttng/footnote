#[test]
fn cause_rows() {
    let stderr = "fno config: a is not modeled\nfno config: b is not modeled\ngh: API rate limit exceeded for user ID 4994564. (HTTP 403)";
    assert_eq!(
        stderr_cause(stderr),
        "gh: API rate limit exceeded for user ID 4994564. (HTTP 403)"
    );

    let error = "gh api repos/{owner}/{repo}/commits/<sha>/check-runs failed: fno config: x is not modeled\ngh: API rate limit exceeded (HTTP 403)";
    assert_eq!(
        gh_error_cause(error),
        "gh: API rate limit exceeded (HTTP 403)"
    );

    assert_eq!(
        stderr_cause("fno config: first\nfno config: last"),
        "fno config: last"
    );
    assert_eq!(stderr_cause(" \n\t"), "no stderr");

    let cause = "é".repeat(300);
    let result = stderr_cause(&cause);
    assert_eq!(result.chars().count(), 120);
    assert_eq!(result, "é".repeat(120));
}

#[test]
fn count_rows() {
    let first = Value::Array((0..100).map(|n| json!({"number": n})).collect());
    let second = Value::Array((100..107).map(|n| json!({"number": n})).collect());
    assert_eq!(
        open_pr_total(&[first, second, json!({"unexpected": true}), json!([1, 2])]),
        109
    );

    assert_eq!(sanitize_scope_key("fno-x-aaaa epic"), "fno-x-aaaa-epic");
    assert_eq!(sanitize_scope_key("  --x--  "), "x");
    assert_eq!(sanitize_scope_key("///"), "");
    assert_eq!(sanitize_scope_key("a, b"), "a-b");
    let telemetry = json!({"status":"zero_rows", "rows_last_hour":0, "live_claude_workers":1});
    let data = build_data(&[Reading::took("telemetry", telemetry.clone())], "fno");
    assert_eq!(data["telemetry"], telemetry);
    assert!(crate::otel_read::health_line(&data["telemetry"]).contains("0 API rows"));
}

// AC4: a correction_meter reading renders one self_correction line naming
// both windows, and the beat data carries the flat newest-window keys.
#[test]
fn correction_rows() {
    // correction_meter is not in the seeded fixture set, so the reading
    // rows push directly.
    let mut readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
    readings.push(
        Reading::took(
            "correction_meter",
            json!({
                "windows_total": 2,
                "self_caught_window": 0,
                "operator_caught_window": 1,
                "recent": [
                    {"window": 1, "self_caught": 1, "operator_caught": 0, "peer_caught": 0, "interrupts": 0, "rejects": 0},
                    {"window": 2, "self_caught": 0, "operator_caught": 1, "peer_caught": 0, "interrupts": 2, "rejects": 1},
                ],
            }),
        ),
    );
    let data = build_data(&readings, "x-bbbb");
    assert_eq!(data["self_caught_window"], json!(0));
    assert_eq!(data["operator_caught_window"], json!(1));
    assert_eq!(data["correction_windows"].as_array().map(Vec::len), Some(2));
    let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
    let line = lines
        .iter()
        .find(|l| l.starts_with("self_correction:"))
        .expect("self_correction line prints");
    assert_eq!(
        line,
        "self_correction: window 2 (open) self 0 / operator 1 (interrupts 2, rejects 1); window 1 self 1 / operator 0"
    );

    // AC5: a failed reading prints the READER FAILED line and no
    // self_correction line.
    let mut readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
    readings.push(Reading::failed(
        "correction_meter",
        "transcript unreadable: gone".to_string(),
    ));
    let data = build_data(&readings, "x-bbbb");
    let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
    assert!(
        lines
            .iter()
            .any(|l| l == "READER FAILED correction_meter: transcript unreadable: gone"),
        "lines: {lines:?}"
    );
    assert!(!lines.iter().any(|l| l.starts_with("self_correction:")));
}
