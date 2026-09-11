//! Stop/rm claim release (x-9c91 change 5): the response-keyed release gate.

use super::*;

/// The pid no OS reports as alive (rm_refusal.rs uses the same token).
const GONE_PID: i64 = i32::MAX as i64;

fn write_stop_claim(
    dir: &std::path::Path,
    key: &str,
    holder: &str,
    pid: i64,
    session: Option<&str>,
) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let body = json!({
        "schema_version": 1,
        "key": key,
        "holder": holder,
        "acquired_at": 1,
        "pid": pid,
        "host": "",
        "machine_id": crate::claims::machine_id(),
        "expires_at": Value::Null,
        "pid_provenance": "session-prover",
        "session_id": session,
    });
    let path = dir.join(format!("{}.lock", crate::claims::encode_key(key)));
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// AC5-EDGE: a `stopped: false` response (a codex interrupt that did not
/// settle) releases nothing: no claim file changes and the response carries
/// no `claims` key.
#[tokio::test]
async fn a_stopped_false_response_releases_nothing() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("stopclaimsfalse");
    let mut row = claude_rm_row("w1", "aaabbb21", "aaabbb21-1111-2222-3333-444444444444");
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let claims_root = home.root().join("claims-root");
    std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
    std::env::set_var("FNO_SPACES_DIR", home.root().join("spaces"));
    let claim_path = write_stop_claim(
        &claims_root.join(".fno/claims"),
        "node:x-edge",
        "spawn-handover:w1",
        GONE_PID,
        None,
    );
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.stop", json!({"name": "w1"}));
    let mut response = Response::ok(1, json!({"stopped": false, "backend": "codex-thread"}));
    attach_stopped_claims_release(&ctx, &request, "stop", &mut response).await;

    let result = response.result().unwrap();
    assert!(
        result.get("claims").is_none(),
        "a refused stop carries no claims receipt"
    );
    assert!(claim_path.exists(), "the claim file must stay in place");
    std::env::remove_var("FNO_CLAIMS_ROOT");
    std::env::remove_var("FNO_SPACES_DIR");
    std::fs::remove_dir_all(home.root()).ok();
}

/// The positive twin: a confirmed stop releases the stopped holder's
/// provably-dead claim and rides the receipt on the response under `claims`.
#[tokio::test]
async fn a_confirmed_stop_releases_the_stopped_holders_dead_claims() {
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = short_home("stopclaimsyes");
    let mut row = claude_rm_row("w1", "aaabbb22", "aaabbb22-1111-2222-3333-444444444444");
    row.status = AgentStatus::Live;
    state::update_registry(&home.registry_json(), |registry| registry.entries.push(row)).unwrap();
    let claims_root = home.root().join("claims-root");
    std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
    std::env::set_var("FNO_SPACES_DIR", home.root().join("spaces"));
    let claim_path = write_stop_claim(
        &claims_root.join(".fno/claims"),
        "node:x-stop",
        "spawn-handover:w1",
        GONE_PID,
        None,
    );
    let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
    let request = Request::new(1, "agent.stop", json!({"name": "w1"}));
    let mut response = Response::ok(1, json!({"stopped": true, "short_id": "aaabbb22"}));
    attach_stopped_claims_release(&ctx, &request, "stop", &mut response).await;

    let claims = response
        .result()
        .unwrap()
        .get("claims")
        .cloned()
        .expect("a confirmed stop rides the claims receipt");
    let released = claims.get("released").and_then(Value::as_array).unwrap();
    assert_eq!(released.len(), 1, "{claims}");
    assert_eq!(released[0]["key"], "node:x-stop");
    assert!(!claim_path.exists(), "the dead claim file is gone");
    std::env::remove_var("FNO_CLAIMS_ROOT");
    std::env::remove_var("FNO_SPACES_DIR");
    std::fs::remove_dir_all(home.root()).ok();
}
