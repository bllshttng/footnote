//! The effect gate's store and hook flows. These seed the approvals db's
//! `decisions` table, so they live in tests/, outside the table-ownership
//! scan of src/: the scanner holds `decisions` to backlog/decisions.rs, a
//! name the approvals schema shares with a different table, and the Python
//! store owns the real decision write (`fno inbox approvals decide`).

use fno_agents::effect_gate::{
    default_db_path, hook_request, judge, map_tool_call, open_db, run_op, submit, verdict,
    EffectRequest,
};
use serde_json::json;
use std::path::PathBuf;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fno-effect-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".fno")).unwrap();
    dir
}

fn write_config(root: &std::path::Path, body: &str) {
    std::fs::write(root.join(".fno/config.toml"), body).unwrap();
}

const AUTHORIZED_CONFIG: &str = "\
[approvals.authorized_principals]
\"external.communication\" = [\"sess-1\"]
\"*\" = [\"boss\"]
";

fn sample_request(created: &str, expires: &str) -> EffectRequest {
    EffectRequest {
        request_id: "req-1".to_string(),
        principal_id: "sess-1".to_string(),
        work_order_id: "unclaimed".to_string(),
        attempt_id: "sess-1".to_string(),
        effect_id: "tool-1".to_string(),
        effect_class: "external.communication".to_string(),
        destination: "a@example.com".to_string(),
        action_digest: "deadbeef".to_string(),
        created_at: created.to_string(),
        expires_at: expires.to_string(),
    }
}
/// Serializes the FNO_SPACES_DIR mutations against the crate's own
/// env-mutating tests, the way src's test_env_lock does inside cfg(test).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn seed_approved(conn: &rusqlite::Connection, digest: &str, principal: &str) {
    conn.execute(
        "INSERT INTO decisions (request_digest, deciding_principal_id, decision, decided_at) VALUES (?,?,?,?)",
        rusqlite::params![digest, principal, "approved", "2026-09-29T12:00:00+00:00"],
    )
    .unwrap();
}

/// Submit is idempotent on the request digest and refuses a denied class;
/// the verdict read allows only an approved, unexpired request whose
/// deciding principal is still named by merged config tiers, and refuses
/// everything else.
#[test]
fn submit_and_verdict_enforce_the_authorization_rules() {
    let root = tmp_root("store");
    write_config(&root, AUTHORIZED_CONFIG);
    let conn = open_db(&root.join("approvals.db")).unwrap();
    let request = sample_request("2026-09-29T12:00:00+00:00", "2099-01-01T00:00:00+00:00");
    let now = chrono::Utc::now();

    let digest = submit(&conn, None, &request).unwrap();
    assert_eq!(submit(&conn, None, &request).unwrap(), digest);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let mut denied = sample_request("2026-09-29T12:00:00+00:00", "2099-01-01T00:00:00+00:00");
    denied.request_id = "req-2".to_string();
    denied.effect_id = "tool-2".to_string();
    denied.effect_class = "financial.payment".to_string();
    let err = submit(&conn, None, &denied).unwrap_err();
    assert!(err.starts_with("denied_effect_class"), "{err}");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    assert!(!verdict(&conn, &root, &digest, now).unwrap());
    seed_approved(&conn, &digest, "sess-1");
    assert!(verdict(&conn, &root, &digest, now).unwrap());

    let case = |effect_id: &str, destination: &str| -> String {
        let mut req = sample_request("2026-09-29T12:00:00+00:00", "2099-01-01T00:00:00+00:00");
        req.request_id = format!("req-{effect_id}");
        req.effect_id = effect_id.to_string();
        req.destination = destination.to_string();
        submit(&conn, None, &req).unwrap()
    };

    let wildcard = case("tool-wildcard", "w@example.com");
    seed_approved(&conn, &wildcard, "boss");
    assert!(verdict(&conn, &root, &wildcard, now).unwrap());

    let revoked = case("tool-revoked", "r@example.com");
    seed_approved(&conn, &revoked, "sess-1");
    assert!(verdict(&conn, &root, &revoked, now).unwrap());
    write_config(
        &root,
        "[approvals.authorized_principals]\n\"*\" = [\"boss\"]\n",
    );
    assert!(!verdict(&conn, &root, &revoked, now).unwrap());

    let bare = tmp_root("store-bare");
    write_config(&bare, "[approvals.authorized_principals]\n");
    assert!(!verdict(&conn, &bare, &revoked, now).unwrap());

    write_config(&root, AUTHORIZED_CONFIG);
    let expired = case("tool-expired", "e@example.com");
    seed_approved(&conn, &expired, "sess-1");
    conn.execute(
        "UPDATE requests SET expires_at = '2026-09-29T00:00:00+00:00' WHERE request_digest = ?",
        [&expired],
    )
    .unwrap();
    assert!(!verdict(&conn, &root, &expired, now).unwrap());

    let declined = case("tool-declined", "d@example.com");
    conn.execute(
        "INSERT INTO decisions (request_digest, deciding_principal_id, decision, decided_at) VALUES (?,?,?,?)",
        rusqlite::params![&declined, "sess-1", "declined", "2026-09-29T12:00:00+00:00"],
    )
    .unwrap();
    assert!(!verdict(&conn, &root, &declined, now).unwrap());
}
/// The hook flow: the request digest is stable across unchanged retries, a
/// judge refusal names the class and the approve command and files exactly
/// one request, an approved retry passes, a second session files its own
/// row, a denied class refuses without filing, an unwritable store fails
/// closed, and the authorized-merge ops answer the three receipts.
#[test]
fn hook_and_door_flows_refuse_recover_and_allow() {
    use chrono::TimeZone;

    let root = tmp_root("hook");
    write_config(&root, AUTHORIZED_CONFIG);
    let db = root.join("approvals.db");
    let payload = json!({
        "tool_name": "mcp__claude_ai_Gmail__send_message",
        "tool_input": {"to": "a@example.com", "body": "hi"},
        "session_id": "sess-1",
        "cwd": root.display().to_string(),
        "db": db.display().to_string(),
    });

    let mapped = map_tool_call(
        "mcp__claude_ai_Gmail__send_message",
        &json!({"to": "a@example.com"}),
    )
    .unwrap();
    let now = chrono::Utc.timestamp_opt(1787998200, 0).single().unwrap();
    let a = hook_request("sess-1", "tool-1", &mapped, now);
    assert_eq!(
        a.request_digest(),
        hook_request(
            "sess-1",
            "tool-1",
            &mapped,
            now + chrono::Duration::minutes(3)
        )
        .request_digest()
    );
    let edited = map_tool_call(
        "mcp__claude_ai_Gmail__send_message",
        &json!({"to": "other@example.com"}),
    )
    .unwrap();
    assert_ne!(
        a.request_digest(),
        hook_request("sess-1", "tool-1", &edited, now).request_digest()
    );

    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("FNO_SPACES_DIR", root.join("spaces"));
    let refusal = judge(&payload, &root).unwrap();
    assert!(refusal.contains("external.communication"), "{refusal}");
    assert!(refusal.contains("fno inbox approvals decide"), "{refusal}");
    let conn = open_db(&db).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert!(judge(&payload, &root).is_some());
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let digest: String = conn
        .query_row("SELECT request_digest FROM requests", [], |row| row.get(0))
        .unwrap();
    seed_approved(&conn, &digest, "boss");
    std::env::set_var("FNO_SPACES_DIR", root.join("spaces"));
    assert!(judge(&payload, &root).is_none());

    let other = tmp_root("hook-other");
    std::fs::copy(&db, other.join("approvals.db")).unwrap();
    let other_payload = json!({
        "tool_name": "mcp__claude_ai_Gmail__send_message",
        "tool_input": {"to": "a@example.com", "body": "hi"},
        "session_id": "sess-2",
        "cwd": other.display().to_string(),
        "db": other.join("approvals.db").display().to_string(),
    });
    assert!(judge(&other_payload, &other).is_some());
    std::env::remove_var("FNO_SPACES_DIR");
    let other_conn = open_db(&other.join("approvals.db")).unwrap();
    let rows: i64 = other_conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "the second session files its own row");

    let deny_root = tmp_root("hook-deny");
    let denial = judge(
        &json!({
            "tool_name": "Bash",
            "tool_input": {"command": "stripe charge create --amount 100"},
            "session_id": "sess-1",
            "cwd": deny_root.display().to_string(),
        }),
        &deny_root,
    )
    .unwrap();
    assert!(denial.contains("denied by core policy"), "{denial}");
    assert!(!deny_root.join("approvals.db").exists());

    let closed = tmp_root("hook-closed");
    std::fs::create_dir_all(closed.join("approvals.db")).unwrap();
    let refusal = judge(
        &json!({
            "tool_name": "mcp__x__send_message",
            "tool_input": {"to": "a@example.com"},
            "session_id": "sess-1",
            "cwd": closed.display().to_string(),
            "db": closed.join("approvals.db").display().to_string(),
        }),
        &closed,
    )
    .unwrap();
    assert!(refusal.contains("unreadable"), "{refusal}");

    // The default approvals path routes through the state-layout resolver:
    // a legacy root file still reads, the migrated db twin wins, and the
    // resolver never moves or creates either file.
    let layout = tmp_root("layout");
    let state = layout.join("state");
    std::fs::create_dir_all(&state).unwrap();
    struct RestoreStateDir(Option<std::ffi::OsString>);
    impl Drop for RestoreStateDir {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("FNO_STATE_DIR", value),
                None => std::env::remove_var("FNO_STATE_DIR"),
            }
        }
    }
    let restore_state_dir = RestoreStateDir(std::env::var_os("FNO_STATE_DIR"));
    std::env::set_var("FNO_STATE_DIR", &state);
    std::fs::write(state.join("approvals.db"), b"legacy").unwrap();
    assert_eq!(
        default_db_path(std::path::Path::new(".")).unwrap(),
        state.join("approvals.db"),
        "an unmigrated root keeps reading the legacy approvals db"
    );
    std::fs::create_dir_all(state.join("db")).unwrap();
    std::fs::write(state.join("db/approvals.db"), b"migrated").unwrap();
    assert_eq!(
        default_db_path(std::path::Path::new(".")).unwrap(),
        state.join("db/approvals.db"),
        "a migrated root reads the db twin"
    );
    assert!(state.join("approvals.db").exists(), "root file untouched");
    drop(restore_state_dir);

    let out = run_op(
        "effect-classify",
        &json!({"effect_class": "financial.payment"}),
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["disposition"],
        "deny"
    );
    let ops = tmp_root("ops");
    let out = run_op(
        "effect-verdict",
        &json!({
            "tool_name": "Bash",
            "tool_input": {"command": "gh repo delete x/y"},
            "cwd": ops.display().to_string(),
        }),
    );
    let receipt = serde_json::from_str::<serde_json::Value>(&out).unwrap();
    assert_eq!(receipt["verdict"], "deny");
    assert_eq!(receipt["effect_class"], "infrastructure.destructive");
    let out = run_op(
        "effect-verdict",
        &json!({"tool_name": "Read", "tool_input": {}, "cwd": ops.display().to_string()}),
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["verdict"],
        "allow"
    );
    assert!(run_op("effect-nonsense", &json!({})).contains("unknown op"));
}
