//! Tests for the native `fno doctor update` verb. Unit-shaped: pure parsing,
//! marker IO, guard parsing, and verdict-row folding. Anything that shells to
//! cargo/uv belongs to the manual verify pass on a scratch CARGO_INSTALL_ROOT.

use serde_json::Value;
use std::path::PathBuf;

use crate::doctor_update::*;

fn os(args: &[&str]) -> Vec<std::ffi::OsString> {
    args.iter().map(std::ffi::OsString::from).collect()
}

#[test]
fn classify_claims_doctor_update_and_root_update() {
    assert_eq!(
        classify(&os(&["doctor", "update", "--check"])),
        Some(os(&["--check"]))
    );
    assert_eq!(classify(&os(&["update", "-N"])), Some(os(&["-N"])));
    assert_eq!(classify(&os(&["update"])), Some(os(&[])));
    // Every other `doctor` subcommand still forwards to Python.
    assert_eq!(classify(&os(&["doctor", "bundle"])), None);
    assert_eq!(classify(&os(&["doctor"])), None);
    assert_eq!(classify(&os(&["version"])), None);
}

#[test]
fn parse_args_reads_every_flag() {
    let f = parse_args(&os(&[
        "--source",
        "/tmp/src",
        "--dry-run",
        "--force",
        "--rust",
    ]))
    .expect("parses");
    assert_eq!(f.source, Some(PathBuf::from("/tmp/src")));
    assert!(f.dry_run && f.force && f.rust && !f.no_rust && !f.check);

    let f = parse_args(&os(&["-N", "-F", "--no-rust", "--check"])).expect("parses");
    assert!(f.dry_run && f.force && f.no_rust && f.check);

    assert!(parse_args(&os(&["--bogus"])).is_err());
    assert!(parse_args(&os(&["--source"])).is_err());
}

#[test]
fn guard_reads_target_state_frontmatter() {
    let _g = crate::model_catalog::state_env_lock();
    let tmp = std::env::temp_dir().join(format!("fno-du-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".fno")).unwrap();
    std::env::set_var("FNO_REPO_ROOT", &tmp);

    std::fs::write(
        tmp.join(".fno").join("target-state.md"),
        "---\nstatus: IN_PROGRESS\n---\nbody\n",
    )
    .unwrap();
    assert!(target_in_progress());

    std::fs::write(
        tmp.join(".fno").join("target-state.md"),
        "---\nstatus: COMPLETE\n---\nbody\n",
    )
    .unwrap();
    assert!(!target_in_progress());

    // No file: the gate is open.
    std::fs::remove_file(tmp.join(".fno").join("target-state.md")).unwrap();
    assert!(!target_in_progress());

    // Unreadable hides an active loop: fail safe as IN_PROGRESS.
    std::fs::write(
        tmp.join(".fno").join("target-state.md"),
        "---\nstatus: IN_PROGRESS\n---\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            tmp.join(".fno").join("target-state.md"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        assert!(target_in_progress());
        std::fs::set_permissions(
            tmp.join(".fno").join("target-state.md"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }

    std::env::remove_var("FNO_REPO_ROOT");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn marker_write_read_round_trip_is_atomic_and_trimmed() {
    let tmp = std::env::temp_dir().join(format!("fno-du-marker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let path = tmp.join("nested").join("installed-rev");
    write_marker(&path, "abc12345").unwrap();
    assert_eq!(read_marker(&path).as_deref(), Some("abc12345"));
    // A missing marker reads None, never an empty string.
    assert_eq!(read_marker(&tmp.join("absent")), None);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn file_eq_compares_bytes_not_metadata() {
    let tmp = std::env::temp_dir().join(format!("fno-du-fileeq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let a = tmp.join("a");
    let b = tmp.join("b");
    std::fs::write(&a, b"same bytes").unwrap();
    std::fs::write(&b, b"same bytes").unwrap();
    assert!(file_eq(&a, &b));
    std::fs::write(&b, b"other bytes").unwrap();
    assert!(!file_eq(&a, &b));
    assert!(!file_eq(&a, &tmp.join("missing")));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn component_lines_extract_only_non_fresh_rows() {
    let report: Value = serde_json::json!({
        "converged": false,
        "components": [
            {"component": "fno-agents", "status": "fresh", "line": "fresh line"},
            {"component": "fno-agents-daemon", "status": "stale", "line": "daemon is stale"},
            {"component": "fno", "status": "updated", "line": "updated line"},
            {"component": "fno-agents-worker", "status": "absent", "line": "worker absent"}
        ]
    });
    let lines = component_lines(Some(&report), "fno doctor update");
    assert_eq!(
        lines,
        vec![
            "fno doctor update: daemon is stale",
            "fno doctor update: worker absent"
        ]
    );
    // A None report carries no rows: the call site names the path instead.
    assert!(component_lines(None, "fno doctor update").is_empty());
}

#[test]
fn row_status_finds_named_components() {
    let report: Value = serde_json::json!({
        "components": [
            {"component": "fno-agents", "status": "stale"}
        ]
    });
    assert_eq!(row_status(&report, "fno-agents").as_deref(), Some("stale"));
    assert_eq!(row_status(&report, "fno"), None);
}

#[test]
fn triad_names_carry_exe_suffix_on_windows() {
    let names = triad_names();
    assert_eq!(names.len(), 3);
    if cfg!(windows) {
        assert!(names[0].ends_with(".exe"));
    } else {
        assert_eq!(names[0], "fno-agents");
    }
}

#[test]
fn guidance_release_branch_names_the_upgrade_command() {
    let _lock = crate::model_catalog::state_env_lock();
    let tmp = std::env::temp_dir().join(format!("fno-du-rel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("FNO_STATE_DIR", &tmp);
    let before = std::env::var_os("FNO_AGENTS_BIN");
    std::env::set_var("FNO_AGENTS_BIN", "/usr/bin/false");
    let payload = update_readiness(None);
    let guidance = payload
        .get("guidance")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    std::env::remove_var("FNO_STATE_DIR");
    match before {
        Some(v) => std::env::set_var("FNO_AGENTS_BIN", v),
        None => std::env::remove_var("FNO_AGENTS_BIN"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        guidance.contains("uv tool upgrade fno"),
        "guidance: {guidance}"
    );
    assert!(!guidance.contains("treated as a wire bump"));
}

#[test]
fn stale_sessions_fold_flags_only_live_stale_rows() {
    let rows: Vec<Value> = serde_json::from_str::<Vec<Value>>(
        r#"[
            {"session": "old", "state": "live", "stale": true},
            {"session": "cur", "state": "live", "stale": false},
            {"session": "pre", "state": "live", "stale": true, "wire_version": null},
            {"session": "dead", "state": "stale", "stale": true}
        ]"#,
    )
    .unwrap();
    // Only LIVE + stale rows; a current-wire live server and a dead socket
    // are excluded (the ported test_mux_staleness contract).
    assert_eq!(stale_sessions_from_rows(&rows), vec!["old", "pre"]);
    assert!(stale_sessions_from_rows(&[]).is_empty());
}

#[test]
fn run_refuses_during_an_in_progress_target() {
    let _lock = crate::model_catalog::state_env_lock();
    let tmp = std::env::temp_dir().join(format!("fno-du-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".fno")).unwrap();
    std::fs::write(
        tmp.join(".fno").join("target-state.md"),
        "---\nstatus: IN_PROGRESS\n---\nbody\n",
    )
    .unwrap();
    std::env::set_var("FNO_REPO_ROOT", &tmp);
    // The guard fires before any source-pin subprocess: no --force, exit 1.
    let code = run(&os(&["--no-rust"]));
    std::env::remove_var("FNO_REPO_ROOT");
    let _ = std::fs::remove_dir_all(&tmp);
    assert_eq!(code, 1);
}

fn git_ok(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn source_sync_fast_forwards_or_refuses_naming_the_gap() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("r.git");
    let (canonical, peer) = (tmp.path().join("c"), tmp.path().join("p"));
    let path = |p: &PathBuf| p.to_string_lossy().into_owned();
    git_ok(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", &path(&remote)],
    );
    git_ok(tmp.path(), &["clone", "-q", &path(&remote), &path(&peer)]);
    git_ok(&peer, &["commit", "-q", "--allow-empty", "-m", "base"]);
    git_ok(&peer, &["push", "-q", "origin", "HEAD:main"]);
    git_ok(
        tmp.path(),
        &["clone", "-q", &path(&remote), &path(&canonical)],
    );
    for n in 0..3 {
        git_ok(
            &peer,
            &["commit", "-q", "--allow-empty", "-m", &format!("ahead {n}")],
        );
    }
    git_ok(&peer, &["push", "-q", "origin", "HEAD:main"]);

    sync_source_checkout(&canonical, false).expect("a clean behind checkout fast-forwards");
    assert_eq!(
        git_ok(&canonical, &["rev-parse", "HEAD"]),
        git_ok(&peer, &["rev-parse", "HEAD"])
    );

    git_ok(
        &canonical,
        &["commit", "-q", "--allow-empty", "-m", "local"],
    );
    git_ok(
        &peer,
        &["commit", "-q", "--allow-empty", "-m", "ahead again"],
    );
    git_ok(&peer, &["push", "-q", "origin", "HEAD:main"]);
    let refusal =
        sync_source_checkout(&canonical, false).expect_err("diverged cannot fast-forward");
    assert!(
        refusal.contains("is 1 commit(s) behind origin/main"),
        "{refusal}"
    );
}
