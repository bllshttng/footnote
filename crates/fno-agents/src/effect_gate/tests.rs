//! Tests for the effect gate. One function per contract layer: the pure
//! table (classify, digest parity with the Python function it replaced, the
//! tool map), the store layer (submit idempotency, the verdict read and its
//! revoked-principal refusal), and the hook/door flows.

use super::*;
use serde_json::json;
use std::path::PathBuf;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fno-effect-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".fno")).unwrap();
    dir
}

fn write_config(root: &Path, body: &str) {
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

fn seed_approved(conn: &Connection, digest: &str, principal: &str) {
    conn.execute(
        "INSERT INTO decisions (request_digest, deciding_principal_id, decision, decided_at) \
         VALUES (?,?,?,?)",
        rusqlite::params![digest, principal, "approved", "2026-09-29T12:00:00+00:00"],
    )
    .unwrap();
}

/// The pure-function layer: the class table, digest parity with
/// models.py `canonical_digest`, the isoformat fraction rule the digest
/// depends on, and the tool-call map.
#[test]
fn table_digest_and_tool_map_match_the_python_legs_they_replaced() {
    use chrono::TimeZone;

    // The class table: five denied, two inert, unknown requires approval.
    assert_eq!(classify("financial.payment"), Disposition::Deny);
    assert_eq!(classify("infrastructure.destructive"), Disposition::Deny);
    assert_eq!(classify("internal.draft"), Disposition::Allow);
    assert_eq!(classify("internal.research"), Disposition::Allow);
    assert_eq!(
        classify("external.publication"),
        Disposition::RequireApproval
    );
    assert_eq!(classify("brand.new.class"), Disposition::RequireApproval);

    // Digest parity: expected values computed by models.py canonical_digest
    // on 2026-09-29. Sorted keys, raw UTF-8, nested maps and arrays.
    let fixture1 = json!({
        "attempt_id": "a1",
        "destination": "ana@example.com",
        "effect_class": "external.communication",
        "effect_id": "t1",
        "expires_at": "2026-09-30T07:55:01+00:00",
        "principal_id": "s1",
        "action_digest": "abc123",
        "work_order_id": "unclaimed",
    });
    assert_eq!(
        canonical_digest(&fixture1),
        "c7d7d226045aefd144302e49e1e8f881ae48d1b3f44431926b6da754625186eb"
    );
    assert_eq!(
        canonical_digest(&json!({"b": "two", "a": "one"})),
        "8f770258ab53f8b20001e6ba82ae42d66479db3053a3b74776bafa2a92674514"
    );
    assert_eq!(
        canonical_digest(
            &json!({"greeting": "héllo wörld ✓", "nested": {"z": 1, "y": [2, "three"]}})
        ),
        "8c8a4ce3dbbe72df5c767bce2df133bb42373dbc5b384f5e662fdce043a920aa"
    );

    // Python isoformat() omits the fraction when the microsecond is zero;
    // bound timestamps ride the digest, so the rule matters.
    let whole = chrono::Utc.timestamp_opt(1788000000, 0).single().unwrap();
    assert!(isoformat(whole).ends_with("+00:00"));
    assert!(!isoformat(whole).contains('.'));
    let micros = chrono::Utc
        .timestamp_opt(1788000000, 123_000)
        .single()
        .unwrap();
    assert!(isoformat(micros).contains(".000123"));

    // The tool map: MCP sends and publishes, Bash senders, remote deletes
    // and payments; reads, local edits and merge verbs map to no effect.
    let gmail = map_tool_call(
        "mcp__claude_ai_Gmail__send_message",
        &json!({"to": "a@example.com", "body": "hi"}),
    )
    .unwrap();
    assert_eq!(gmail.effect_class, "external.communication");
    assert_eq!(gmail.destination, "a@example.com");

    let slack = map_tool_call(
        "mcp__slack__post_message",
        &json!({"channel_id": "c1", "text": "hi"}),
    )
    .unwrap();
    assert_eq!(slack.effect_class, "external.communication");
    assert_eq!(slack.destination, "c1");

    let publish = map_tool_call(
        "mcp__site__publish_page",
        &json!({"site": "docs.example.com"}),
    )
    .unwrap();
    assert_eq!(publish.effect_class, "external.publication");
    assert_eq!(publish.destination, "docs.example.com");

    assert!(map_tool_call("mcp__repo__search", &json!({})).is_none());
    assert!(map_tool_call("Read", &json!({})).is_none());
    assert!(map_tool_call("Bash", &json!({"command": "git status"})).is_none());
    // Merge effects stay with the merge gate and git-protection.
    assert!(map_tool_call("Bash", &json!({"command": "gh pr merge 12"})).is_none());
    assert!(map_tool_call("Bash", &json!({"command": "git push origin main"})).is_none());

    let send = map_tool_call(
        "Bash",
        &json!({"command": "gws gmail send --to b@example.com -s hi"}),
    )
    .unwrap();
    assert_eq!(send.effect_class, "external.communication");
    assert_eq!(send.destination, "b@example.com");

    assert_eq!(
        map_tool_call("Bash", &json!({"command": "gh issue comment 5 -b x"}))
            .unwrap()
            .effect_class,
        "external.communication"
    );
    assert_eq!(
        map_tool_call(
            "Bash",
            &json!({"command": "gh repo delete bllshttng/x --yes"})
        )
        .unwrap()
        .effect_class,
        "infrastructure.destructive"
    );
    assert_eq!(
        map_tool_call("Bash", &json!({"command": "aws s3 rm s3://b/x"}))
            .unwrap()
            .effect_class,
        "infrastructure.destructive"
    );
    assert_eq!(
        map_tool_call(
            "Bash",
            &json!({"command": "gcloud projects delete p --quiet"})
        )
        .unwrap()
        .effect_class,
        "infrastructure.destructive"
    );
    assert_eq!(
        map_tool_call(
            "Bash",
            &json!({"command": "stripe charge create --amount 100"})
        )
        .unwrap()
        .effect_class,
        "financial.payment"
    );
}

/// The store layer: submit is idempotent on the request digest and refuses a
/// denied class; the verdict read allows only an approved, unexpired request
/// whose deciding principal is still named by config, and refuses everything
/// else including an unreadable config.
#[test]
fn submit_and_verdict_enforce_the_authorization_rules() {
    let root = tmp_root("store");
    write_config(&root, AUTHORIZED_CONFIG);
    let conn = open_db(&root.join("approvals.db")).unwrap();
    let request = sample_request("2026-09-29T12:00:00+00:00", "2099-01-01T00:00:00+00:00");
    let now = chrono::Utc::now();

    // Idempotency: the identical request twice leaves one row.
    let digest = submit(&conn, None, &request).unwrap();
    assert_eq!(submit(&conn, None, &request).unwrap(), digest);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    // A denied class never becomes a row.
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

    // No decision yet: refused.
    assert!(!verdict(&conn, &root, &digest, now).unwrap());
    seed_approved(&conn, &digest, "sess-1");
    // Approved by an authorized principal, unexpired: allowed.
    assert!(verdict(&conn, &root, &digest, now).unwrap());
    // The wildcard class also authorizes: boss is named only under `*`.
    conn.execute(
        "UPDATE decisions SET deciding_principal_id = 'boss' WHERE request_digest = ?",
        [&digest],
    )
    .unwrap();
    assert!(verdict(&conn, &root, &digest, now).unwrap());
    // A principal removed from config after the decision is refused.
    conn.execute(
        "UPDATE decisions SET deciding_principal_id = 'sess-1' WHERE request_digest = ?",
        [&digest],
    )
    .unwrap();
    write_config(
        &root,
        "[approvals.authorized_principals]\n\"*\" = [\"boss\"]\n",
    );
    assert!(!verdict(&conn, &root, &digest, now).unwrap());

    // With no policy configured at all, nobody is authorized.
    let bare = tmp_root("store-bare");
    write_config(&bare, "[approvals.authorized_principals]\n");
    assert!(!verdict(&conn, &bare, &digest, now).unwrap());

    // Expired: refused.
    write_config(&root, AUTHORIZED_CONFIG);
    conn.execute(
        "UPDATE requests SET expires_at = '2026-09-29T00:00:00+00:00' WHERE request_digest = ?",
        [&digest],
    )
    .unwrap();
    assert!(!verdict(&conn, &root, &digest, now).unwrap());
    // Declined: refused.
    conn.execute(
        "UPDATE requests SET expires_at = '2099-01-01T00:00:00+00:00' WHERE request_digest = ?",
        [&digest],
    )
    .unwrap();
    conn.execute(
        "UPDATE decisions SET decision = 'declined' WHERE request_digest = ?",
        [&digest],
    )
    .unwrap();
    assert!(!verdict(&conn, &root, &digest, now).unwrap());
}

/// The hook and door flows: the request digest is stable across unchanged
/// retries, a judge refusal names the class and the approve command and
/// files exactly one request, an approved retry passes, a denied class
/// refuses without filing, an unwritable store fails closed, and the
/// authorized-merge ops answer the three receipts.
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

    // Digest stability: the digest must be STABLE across unchanged retries,
    // so the expiry is floored to the hour. Minute 10 of some hour, so a
    // +3m retry cannot cross the boundary.
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
    // An edited call changes the action digest and so the request digest.
    let edited = map_tool_call(
        "mcp__claude_ai_Gmail__send_message",
        &json!({"to": "other@example.com"}),
    )
    .unwrap();
    assert_ne!(
        a.request_digest(),
        hook_request("sess-1", "tool-1", &edited, now).request_digest()
    );

    // First fire: denied, one request filed, the approve command named.
    let _env = crate::claims::test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var("FNO_SPACES_DIR", root.join("spaces"));
    let refusal = judge(&payload, &root).unwrap();
    assert!(refusal.contains("external.communication"), "{refusal}");
    assert!(refusal.contains("fno inbox approvals decide"), "{refusal}");
    let conn = open_db(&db).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    // Retry unchanged while still unapproved: refused again, still one row.
    assert!(judge(&payload, &root).is_some());
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    // An authorized principal approves; the unchanged retry now passes.
    let digest: String = conn
        .query_row("SELECT request_digest FROM requests", [], |row| row.get(0))
        .unwrap();
    seed_approved(&conn, &digest, "boss");
    assert!(judge(&payload, &root).is_none());
    std::env::remove_var("FNO_SPACES_DIR");

    // A second session making the same call files its own request: request_id
    // derives from the request digest, so the rows never clash on UNIQUE.
    let other = tmp_root("hook-other");
    std::fs::copy(&db, other.join("approvals.db")).unwrap();
    let other_payload = json!({
        "tool_name": "mcp__claude_ai_Gmail__send_message",
        "tool_input": {"to": "a@example.com", "body": "hi"},
        "session_id": "sess-2",
        "cwd": other.display().to_string(),
        "db": other.join("approvals.db").display().to_string(),
    });
    // The env lock taken for the first judge call still guards this scope;
    // taking it twice on one thread would deadlock.
    std::env::set_var("FNO_SPACES_DIR", other.join("spaces"));
    assert!(judge(&other_payload, &other).is_some());
    std::env::remove_var("FNO_SPACES_DIR");
    let other_conn = open_db(&other.join("approvals.db")).unwrap();
    let rows: i64 = other_conn
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "the second session files its own row");

    // A denied class refuses without filing anything.
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

    // An unwritable store fails closed.
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

    // The door ops: classify, a deny verdict for a destructive call, a
    // no-effect allow, and the unknown-op error.
    let out = run_op(
        "effect-classify",
        &json!({"effect_class": "financial.payment"}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["disposition"],
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
    let receipt = serde_json::from_str::<Value>(&out).unwrap();
    assert_eq!(receipt["verdict"], "deny");
    assert_eq!(receipt["effect_class"], "infrastructure.destructive");
    let out = run_op(
        "effect-verdict",
        &json!({"tool_name": "Read", "tool_input": {}, "cwd": ops.display().to_string()}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["verdict"],
        "allow"
    );
    assert!(run_op("effect-nonsense", &json!({})).contains("unknown op"));
}
