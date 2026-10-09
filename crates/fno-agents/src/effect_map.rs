//! The effect table's pure layer: the class dispositions, the canonical
//! digest, and the tool-call map. `effect_gate` owns the approvals store and
//! re-exports this table; the footnote harness reads the map through the
//! fno crate's generated copy, so one table classifies every harness.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The class table's dispositions, mirroring `EffectDisposition` in models.py.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Allow,
    RequireApproval,
    Deny,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Allow => "allow",
            Disposition::RequireApproval => "require_approval",
            Disposition::Deny => "deny",
        }
    }
}

/// Refused until a later explicit policy and adapter contract exist
/// (models.py DENIED_EFFECT_CLASSES).
const DENIED_EFFECT_CLASSES: [&str; 5] = [
    "financial.payment",
    "financial.commitment",
    "signature.contract",
    "employment.action",
    "infrastructure.destructive",
];

/// No external consequence, so no effect approval (models.py INERT_EFFECT_CLASSES).
const INERT_EFFECT_CLASSES: [&str; 2] = ["internal.draft", "internal.research"];

/// Classify an effect class. Function-agnostic: only the class is read.
pub fn classify(effect_class: &str) -> Disposition {
    if DENIED_EFFECT_CLASSES.contains(&effect_class) {
        return Disposition::Deny;
    }
    if INERT_EFFECT_CLASSES.contains(&effect_class) {
        return Disposition::Allow;
    }
    Disposition::RequireApproval
}

/// Digest a mapping so any change to any bound field changes the digest.
/// Byte-identical to models.py `canonical_digest`: sorted keys, `(",", ":")`
/// separators, raw UTF-8 (ensure_ascii=False).
pub fn canonical_digest(value: &Value) -> String {
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(*k).unwrap_or_default());
                    out.push(':');
                    write(&map[*k], out);
                }
                out.push('}');
            }
        }
    }
    let mut canonical = String::new();
    write(value, &mut canonical);
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

/// Python `datetime.isoformat()` on an aware UTC datetime: no fraction when
/// the microsecond is zero, `+00:00` suffix either way.
pub(crate) fn isoformat(t: chrono::DateTime<chrono::Utc>) -> String {
    if t.timestamp_subsec_micros() == 0 {
        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    } else {
        t.to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
    }
}

// -- Tool call to effect class ----------------------------------------------

/// One tool call mapped to the effect class it would exercise.
pub struct MappedEffect {
    pub effect_class: &'static str,
    pub destination: String,
    pub action_digest: String,
}

/// First present input field among the candidates, else `unspecified`.
fn destination_from(input: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(v) = input.get(*key).and_then(Value::as_str) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "unspecified".to_string()
}

fn mapped(class: &'static str, destination: String, tool: &str, input: &Value) -> MappedEffect {
    MappedEffect {
        effect_class: class,
        destination,
        action_digest: canonical_digest(&json!({"tool": tool, "input": input})),
    }
}

/// Map one tool call to its effect class, or None when the call carries none.
/// `gh pr merge` and `git push` map to None on purpose: the merge gate and
/// git-protection own them, and this guard never decides them twice.
pub fn map_tool_call(tool_name: &str, tool_input: &Value) -> Option<MappedEffect> {
    if tool_name == "Bash" {
        return map_bash_command(
            tool_input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
    }
    let lower = tool_name.to_lowercase();
    if !tool_name.starts_with("mcp__") {
        return None;
    }
    if lower.contains("send") || lower.contains("post_message") {
        return Some(mapped(
            "external.communication",
            destination_from(
                tool_input,
                &[
                    "to",
                    "recipient",
                    "email",
                    "channel",
                    "channel_id",
                    "room",
                    "user",
                    "username",
                    "phone",
                    "webhook",
                ],
            ),
            tool_name,
            tool_input,
        ));
    }
    if lower.contains("publish") || lower.contains("create_post") || lower.contains("deploy") {
        return Some(mapped(
            "external.publication",
            destination_from(
                tool_input,
                &["site", "project", "target", "url", "domain", "page", "name"],
            ),
            tool_name,
            tool_input,
        ));
    }
    None
}

/// The Bash rows of the effect table. Tokens are matched on the command's
/// leading words; anything else is no effect and is never blocked here.
pub fn map_bash_command(command: &str) -> Option<MappedEffect> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let input = json!({ "command": command });
    // Merge effects belong to the merge gate and git-protection. A PR or
    // issue comment is part of the review loop, like `gh pr create` and the
    // `gh api` review comments: no person outside the repo receives it.
    if matches!(tokens.as_slice(), ["gh", "pr", "merge", ..])
        || matches!(tokens.as_slice(), ["git", "push", ..])
    {
        return None;
    }
    match tokens.as_slice() {
        ["gws", "gmail", "send", ..] => {
            let destination = tokens
                .iter()
                .position(|t| *t == "--to")
                .and_then(|i| tokens.get(i + 1))
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unspecified".to_string());
            Some(mapped(
                "external.communication",
                destination,
                "Bash",
                &input,
            ))
        }
        ["gh", "repo", "delete", ..]
        | ["gh", "release", "delete", ..]
        | ["aws", "s3", "rm", ..] => Some(mapped(
            "infrastructure.destructive",
            "remote".to_string(),
            "Bash",
            &input,
        )),
        ["gcloud", ..] if tokens.iter().any(|t| *t == "delete") => Some(mapped(
            "infrastructure.destructive",
            "gcp".to_string(),
            "Bash",
            &input,
        )),
        ["stripe", verb, ..] if matches!(*verb, "charge" | "payout" | "transfer") => Some(mapped(
            "financial.payment",
            "stripe".to_string(),
            "Bash",
            &input,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
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

        // A PR or issue comment is review traffic, not outside communication.
        assert!(map_tool_call("Bash", &json!({"command": "gh issue comment 5 -b x"})).is_none());
        assert!(map_tool_call("Bash", &json!({"command": "gh pr comment 5 -b x"})).is_none());
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
}
