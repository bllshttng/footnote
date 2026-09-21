//! The corrections-pointer tests, in their own file so finalize.rs stays
//! under the 5000-line budget: the pointer writer now carries the
//! create-at-0600 behavior plus the real-run fixture guard, and both test
//! shapes live beside each other here.

use super::*;

#[test]
fn corrections_pointer_refuses_temp_dir_postmortem() {
    // AC1-EDGE: the 360-fixture-row shape. A postmortem under a per-test
    // temp dir with the log resolved through a real home appends NOTHING;
    // the log must be byte-identical afterwards. Uses a sibling of the
    // accepted root, not a /tmp name match (AC1-ERR).
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fno_home = std::env::temp_dir().join(format!("fin-corr-fx-{}", std::process::id()));
    let home = std::env::temp_dir().join(format!("fin-corr-fxh-{}", std::process::id()));
    let _ = fs::create_dir_all(&fno_home);
    let _ = fs::create_dir_all(&home);
    let log_path = fno_home.join("corrections.log");
    fs::write(&log_path, "").unwrap();
    let fixture_pm = fno_home.join("pm-sibling-not-postmortems").join("pm.md");

    std::env::remove_var(CORRECTIONS_LOG_ENV);
    std::env::set_var("FNO_HOME", &fno_home);
    append_corrections_pointer(Some(&home), &fixture_pm, "NoProgress", "d");
    std::env::remove_var("FNO_HOME");

    let contents = fs::read_to_string(&log_path).unwrap();
    assert!(
        contents.is_empty(),
        "fixture postmortem must not append: {contents}"
    );
    let _ = fs::remove_dir_all(&fno_home);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn corrections_pointer_creates_absent_log_at_0600() {
    // A termination against a home with no corrections.log yields a
    // one-row log at 0600 instead of a dropped row; a second
    // termination appends without rewriting the mode. The postmortem
    // sits under the real postmortems root so the fixture guard lets
    // it through.
    use std::os::unix::fs::PermissionsExt;
    let _guard = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("fin-corr-create-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let fno_dir = home.join(".fno");
    let pm_dir = fno_dir.join("postmortems");
    fs::create_dir_all(&pm_dir).unwrap();
    let real_pm = pm_dir.join("pm-x.md");
    fs::write(&real_pm, "postmortem").unwrap();
    let log_path = fno_dir.join("corrections.log");

    std::env::remove_var(CORRECTIONS_LOG_ENV);
    std::env::remove_var("FNO_HOME");
    append_corrections_pointer(Some(&home), &real_pm, "NoProgress", "s");

    let contents = fs::read_to_string(&log_path).unwrap();
    assert!(contents.contains("target-postmortem"), "{contents}");
    assert!(contents.contains("pm-x.md"), "{contents}");
    let mode = fs::metadata(&log_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "mode {:o}", mode);

    let real_pm2 = pm_dir.join("pm-y.md");
    fs::write(&real_pm2, "postmortem").unwrap();
    append_corrections_pointer(Some(&home), &real_pm2, "Budget", "d");
    let contents = fs::read_to_string(&log_path).unwrap();
    assert_eq!(contents.lines().count(), 2, "{contents}");
    let mode = fs::metadata(&log_path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "mode {:o}", mode);
    let _ = fs::remove_dir_all(&home);
}
