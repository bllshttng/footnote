//! `fno-agents mail-backfill` - the historical-mail row contract.
//!
//! Mail an outage bypassed (sent over the harness's native cross-session
//! transport while `fno mail send` was down) goes back into the bus log as
//! durable rows that never re-deliver. The pure bodies live here once and
//! Python keeps the transport: the deterministic archive id (the idempotency
//! key), the receiver-side block parser, and the provenance-complete row
//! assembly. The write itself stays Python (`bus.log.record_backfill_delivery`)
//! so the bus keeps its single serializer (AC20-HP).
//!
//! Contract: provenance is the point. A row missing any required fact
//! (sender session, receiver session, time, id, either transcript) refuses
//! loud with exit 2 rather than archiving a half-attributed message.

use serde_json::json;
use std::io::Read as _;

/// The audit-only delivery value: the bytes already reached the recipient
/// over the harness's native cross-session transport. Mirrors
/// `bus.log.CROSS_SESSION_DELIVERY` and the drain gates
/// (`mail_control_drain::AUDIT_ONLY_DELIVERIES`).
pub const CROSS_SESSION_DELIVERY: &str = "cross-session";

/// The transport label the row's meta carries, the node's provenance string.
pub const TRANSPORT: &str = "claude-cross-session";

/// The deterministic archive id: `fmail-bf-` + the first 12 hex of the
/// sha256 over `sender-session|row-uuid`. Stable across re-runs so a
/// re-scan skips what already landed (idempotent by msg_id).
fn archive_msg_id(sender_session: &str, row_uuid: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(sender_session.as_bytes());
    h.update(b"|");
    h.update(row_uuid.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("fmail-bf-{}", &hex[..12])
}

/// Parse every cross-session-message block out of one user-turn text.
/// Attribute vocabulary observed in the wild: `from` (a socket or a name)
/// and `from-name` (the sender's legible label); the body is the text
/// between the open tag's `>` and the closing tag, byte-verbatim.
fn parse_blocks(text: &str) -> Vec<serde_json::Value> {
    let re =
        regex::Regex::new(r#"(?s)<cross-session-message\b([^>]*)>(.*?)</cross-session-message>"#)
            .expect("valid pattern");
    let attr_re = regex::Regex::new(r#"([a-zA-Z-]+)="([^"]*)""#).expect("valid pattern");
    re.captures_iter(text)
        .map(|c| {
            let attrs: serde_json::Map<String, serde_json::Value> = attr_re
                .captures_iter(c.get(1).map(|m| m.as_str()).unwrap_or(""))
                .map(|a| {
                    (
                        a.get(1).unwrap().as_str().to_string(),
                        json!(a.get(2).unwrap().as_str()),
                    )
                })
                .collect();
            let mut out = serde_json::Map::new();
            out.insert(
                "from".into(),
                attrs.get("from").cloned().unwrap_or(json!(null)),
            );
            out.insert(
                "from_name".into(),
                attrs.get("from-name").cloned().unwrap_or(json!(null)),
            );
            out.insert(
                "body".into(),
                json!(c.get(2).map(|m| m.as_str()).unwrap_or("")),
            );
            serde_json::Value::Object(out)
        })
        .collect()
}

/// The required provenance flags, validated before any row is emitted.
const REQUIRED: [&str; 8] = [
    "msg-id",
    "from",
    "to",
    "ts",
    "from-session",
    "to-session",
    "sender-transcript",
    "receiver-transcript",
];

/// Entry: `msgid` | `block` | `row`. `block` and `row` read their text /
/// body from stdin so message bytes never ride argv. A missing required
/// provenance flag is a transport bug: refuse loud on stderr with exit 2,
/// never print a half-attributed row.
pub fn run_mail_backfill(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1.min(args.len())..];
    match sub {
        "msgid" => {
            let get = |k: &str| flag_of(rest, k);
            match (get("sender-session"), get("row-uuid")) {
                (Some(s), Some(u)) => {
                    println!("{}", archive_msg_id(&s, &u));
                    0
                }
                _ => {
                    eprintln!("mail-backfill msgid: --sender-session and --row-uuid are required");
                    2
                }
            }
        }
        "block" => {
            let mut text = String::new();
            if std::io::stdin().read_to_string(&mut text).is_err() {
                eprintln!("mail-backfill block: unreadable stdin");
                return 2;
            }
            println!("{}", serde_json::Value::Array(parse_blocks(&text)));
            0
        }
        "row" => {
            let mut flags: std::collections::HashMap<&str, String> = rest
                .chunks(2)
                .filter(|c| c.len() == 2 && c[0].starts_with("--"))
                .map(|c| (c[0].trim_start_matches('-'), c[1].clone()))
                .collect();
            let mut body = String::new();
            if std::io::stdin().read_to_string(&mut body).is_err() {
                eprintln!("mail-backfill row: unreadable stdin body");
                return 2;
            }
            let missing: Vec<&str> = REQUIRED
                .iter()
                .copied()
                .filter(|k| !flags.contains_key(*k))
                .collect();
            if !missing.is_empty() {
                eprintln!(
                    "mail-backfill row: missing provenance: {}",
                    missing.join(", ")
                );
                return 2;
            }
            let take = |k: &str| flags.remove(k).unwrap_or_default();
            let word_count = body.split_whitespace().count();
            let row = json!({
                "id": take("msg-id"),
                "from_": take("from"),
                "to": take("to"),
                "kind": "send",
                "ts": take("ts"),
                "subject": flags.get("subject").cloned(),
                "from_session": take("from-session"),
                "to_session": take("to-session"),
                "delivery": CROSS_SESSION_DELIVERY,
                "origin": "peer",
                "from_harness": "claude",
                "to_harness": "claude",
                "to_kind": "session",
                "word_count": word_count,
                "meta": {
                    "transport": TRANSPORT,
                    "sender_transcript": take("sender-transcript"),
                    "receiver_transcript": take("receiver-transcript"),
                    "backfilled_at": take("backfilled-at"),
                    "sender_row": flags.get("sender-row").cloned(),
                },
                "body": body,
            });
            println!("{row}");
            0
        }
        other => {
            eprintln!("mail-backfill: unknown subcommand {other:?}");
            2
        }
    }
}

fn flag_of(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == format!("--{name}"))
        .map(|w| w[1].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msgid_is_deterministic_and_input_sensitive() {
        let a = archive_msg_id("sess-1", "uuid-1");
        assert_eq!(a, archive_msg_id("sess-1", "uuid-1"));
        assert_ne!(a, archive_msg_id("sess-2", "uuid-1"));
        assert_ne!(a, archive_msg_id("sess-1", "uuid-2"));
        assert!(a.starts_with("fmail-bf-"));
        assert_eq!(a.len(), "fmail-bf-".len() + 12);
    }

    #[test]
    fn block_parser_reads_attrs_and_verbatim_body() {
        let text = "before <cross-session-message from=\"uds:/tmp/cc-socks/9.sock\" from-name=\"lead\">\nhello\nworld\n</cross-session-message> after";
        let blocks = parse_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["from"], "uds:/tmp/cc-socks/9.sock");
        assert_eq!(blocks[0]["from_name"], "lead");
        assert_eq!(blocks[0]["body"], "\nhello\nworld\n");
    }

    #[test]
    fn block_parser_finds_multiple_blocks_and_skips_plain_text() {
        let two = "<cross-session-message from-name=\"a\">x</cross-session-message>\n<cross-session-message from-name=\"b\">y</cross-session-message>";
        assert_eq!(parse_blocks(two).len(), 2);
        assert!(parse_blocks("no blocks here").is_empty());
    }

    #[test]
    fn row_refuses_missing_provenance_loud() {
        let args: Vec<String> = ["row", "--msg-id", "m1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(run_mail_backfill(&args), 2);
    }
}
