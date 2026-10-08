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
