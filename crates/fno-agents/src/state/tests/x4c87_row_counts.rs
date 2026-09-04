//! x-4c87 raw-versus-decoded row count family: moved verbatim out of state.rs
//! (file budget shrink). Parent helpers resolve through the glob.
use super::*;

// ------------------------------------------------------------------
// x-4c87: the raw-versus-decoded row count invariant. The live outage's
// sanitized shape: real worker rows at the CURRENT schema, one carrying a
// value the typed model cannot represent. (On the machine it was measured
// on, the divergence was versional: a stale v11 daemon meeting the v14
// store; the same-schema unknown-status row below drives the identical
// code path -- typed decode fails with rows on disk.)
// ------------------------------------------------------------------

/// A sanitized 3-row registry whose middle row the typed reader cannot
/// represent. Keys and value types mirror a real worker row; names, ids,
/// and paths are synthetic.
fn divergent_registry_fixture() -> String {
    let row = |name: &str, status: &str| {
        format!(
            r#"{{"name":"{name}","cwd":"/tmp/proj","harness":"claude","harness_session_id":"11111111-2222-3333-4444-555555555555","status":"{status}","created_at":"2026-08-16T00:00:00Z"}}"#
        )
    };
    format!(
        r#"{{"schema_version":{},"agents":[{},{},{}]}}"#,
        REGISTRY_SCHEMA_VERSION,
        row("worker-alpha", "live"),
        row("worker-beta", "hibernating"),
        row("worker-gamma", "live")
    )
}

/// x-d19e wording contract, scoped to text this change introduces: a
/// refusal names what to verify and never advertises an override.
fn assert_no_override_remedy(msg: &str) {
    for banned in ["force", "skip", "ignore", "bypass", "no-verify"] {
        assert!(
            !msg.to_lowercase().contains(banned),
            "diagnostic must not name an override remedy ({banned}): {msg}"
        );
    }
}

#[test]
fn registry_with_unrepresentable_row_errors_naming_both_counts() {
    // AC3-ERR: a positive raw row count whose typed decode fails is an
    // InvariantViolation carrying the path, both counts, and the
    // comparison to run -- never a successful empty roster.
    let dir = tmpdir("divergent-row");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(&path, divergent_registry_fixture()).unwrap();

    let err = load_registry(&path).expect_err("unrepresentable row must fail the read");
    let msg = err.to_string();
    assert!(
        matches!(err, StateError::InvariantViolation(_)),
        "got: {msg}"
    );
    assert!(
        msg.contains(path.to_str().unwrap()),
        "names the path: {msg}"
    );
    assert!(msg.contains("raw_rows=3"), "names the raw count: {msg}");
    assert!(
        msg.contains("decoded_rows=0"),
        "names the decoded count: {msg}"
    );
    assert!(
        msg.contains("inspect"),
        "tells the operator to inspect: {msg}"
    );
    assert!(
        msg.contains("compare"),
        "names the comparison to run: {msg}"
    );
    assert_no_override_remedy(&msg);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_preserves_the_count_of_valid_canonical_rows() {
    // AC2-HP: a valid schema-14 registry round-trips its exact row count.
    let dir = tmpdir("valid-count");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        r#"{"schema_version":14,"agents":[
                {"name":"worker-alpha","cwd":"/tmp/proj","harness":"claude",
                 "status":"live","created_at":"2026-08-16T00:00:00Z"},
                {"name":"worker-gamma","cwd":"/tmp/proj","harness":"codex",
                 "status":"exited","created_at":"2026-08-16T00:00:00Z"}
            ]}"#,
    )
    .unwrap();
    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert_eq!(reg.entries.len(), 2, "both valid rows decode");
    assert_eq!(raw, 2, "raw count agrees with the typed count");
    assert!(reg.find("worker-alpha").is_some());
    assert!(reg.find("worker-gamma").is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_counts_the_legacy_entries_alias_too() {
    // A daemon-written legacy store keys its rows under `entries`; the raw
    // count must follow the same alias the typed decode follows.
    let dir = tmpdir("legacy-entries");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        r#"{"schema_version":14,"entries":[
                {"name":"legacy-one","cwd":"/tmp/proj","harness":"claude",
                 "status":"live","created_at":"2026-08-16T00:00:00Z"},
                {"name":"legacy-two","cwd":"/tmp/proj","harness":"claude",
                 "status":"live","created_at":"2026-08-16T00:00:00Z"}
            ]}"#,
    )
    .unwrap();
    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert_eq!(reg.entries.len(), 2);
    assert_eq!(raw, 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_missing_agents_key_is_a_valid_empty() {
    // A store with no row array at all is a valid zero-agent registry (the
    // raw count is 0; nothing could have been lost).
    let dir = tmpdir("no-array-key");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(&path, r#"{"schema_version":14}"#).unwrap();
    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert!(reg.entries.is_empty());
    assert_eq!(raw, 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_true_empty_states_stay_successful_zeros() {
    // AC4-EDGE: missing file, whitespace-only, and a real empty array are
    // valid zero-agent states. No check treats byte length as evidence of
    // rows.
    let dir = tmpdir("true-empties");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");

    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert!(reg.entries.is_empty(), "missing file is a valid empty");
    assert_eq!(raw, 0);

    std::fs::write(&path, "   \n\t ").unwrap();
    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert!(reg.entries.is_empty(), "whitespace file is a valid empty");
    assert_eq!(raw, 0);

    std::fs::write(&path, r#"{"schema_version":14,"agents":[]}"#).unwrap();
    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert!(reg.entries.is_empty(), "an empty array is a valid empty");
    assert_eq!(raw, 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_typed_failure_without_rows_keeps_the_parse_error() {
    // A parse failure with NO rows on disk (here: schema_version of the
    // wrong type) is a plain parse error, not a row-loss divergence -- the
    // guard is keyed to a positive raw count, never to byte length.
    let dir = tmpdir("no-rows-failure");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(&path, r#"{"schema_version":"fourteen","agents":[]}"#).unwrap();
    let err = load_registry(&path).expect_err("wrong-typed schema_version must fail");
    assert!(
        !matches!(err, StateError::InvariantViolation(_)),
        "no rows existed to lose: {err}"
    );
    assert!(!err.to_string().contains("raw_rows"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_future_schema_partial_read_exposes_both_counts() {
    // Forward policy preserved (see a_newer_row_with_an_unknown_status_
    // skips_that_row_only): a future-schema store still degrades per-row
    // with an announcement. What changes is that the read now CARRIES the
    // raw count, so the daemon's startup assertion can refuse to serve
    // the 1-of-2 partial as a complete roster.
    let dir = tmpdir("future-partial");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(
        &path,
        // One past THIS binary, derived so a schema bump cannot quietly
        // turn the future-schema fixture into a current-schema one and
        // leave the test asserting a condition it no longer sets up.
        format!(
            r#"{{"schema_version":{},"agents":[
                {{"name":"future","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"hibernating","created_at":"2026-01-01T00:00:00Z"}},
                {{"name":"readable","cwd":"/x","log_path":"/l","harness":"claude",
                 "status":"live","created_at":"2026-01-01T00:00:00Z"}}
            ]}}"#,
            REGISTRY_SCHEMA_VERSION + 1
        ),
    )
    .unwrap();
    let (reg, raw) = load_registry_with_counts(&path).unwrap();
    assert_eq!(reg.entries.len(), 1, "the representable row still decodes");
    assert_eq!(raw, 2, "the raw count says what was dropped");
    std::fs::remove_dir_all(&dir).ok();
}
