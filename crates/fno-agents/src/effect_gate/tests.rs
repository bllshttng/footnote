//! Tests for the effect gate's pure-function layer: the class table, digest
//! parity with the Python function it replaced, the isoformat fraction rule
//! the digest depends on, and the tool-call map. The store and hook flows
//! seed an approvals db, so they live in tests/effect_gate_flows.rs, outside
//! the table-ownership scan of src/.

use super::*;

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
