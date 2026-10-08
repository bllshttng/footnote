use std::process::Command;

#[test]
fn native_doctor_cost_exports_and_reports_without_python() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("agents");
    std::fs::create_dir_all(home.join("otel")).unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(&config, "[telemetry]\nclaude_otel = true\n").unwrap();
    std::fs::write(home.join("registry.json"), r#"{"entries":[]}"#).unwrap();
    let db = home.join("otel/otel.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(fno::otel_read::schema_sql()).unwrap();
    conn.execute("INSERT INTO api_requests (dedupe_key,session_id,ts,model,skill_name,cost_usd_micros) VALUES ('request','session',?1,'model','skill',1500000)", [chrono::Utc::now().to_rfc3339()]).unwrap();
    drop(conn);
    let call = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fno"))
            .args(args)
            .env("FNO_AGENTS_HOME", &home)
            .env("FNO_CONFIG", &config)
            .env("FNO_PY", temp.path().join("no-python"))
            .env("FNO_TEST_HERMETIC", "1")
            .output()
            .unwrap()
    };
    let export = call(&["doctor", "cost", "export", "--csv"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert!(String::from_utf8(export.stdout)
        .unwrap()
        .contains(",session,model,skill,1,0,1.500000"));
    let status = call(&["doctor", "cost", "status", "--json"]);
    assert_eq!(status.status.code(), Some(1));
    let health: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(health["rows_last_hour"], 1);
    assert_eq!(health["status"], "offline");
    std::fs::write(&db, b"invalid sqlite").unwrap();
    let refused = call(&["doctor", "cost", "export", "--csv"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(refused.stdout.is_empty());
}
