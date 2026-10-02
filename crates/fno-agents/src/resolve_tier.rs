//! The tier a session token resolves a registry row at. Owns the token-shape
//! gate and the per-row tier walk so `client_verbs` stays under its file
//! budget; the resolver (`find_agent_entry`) and every caller read them
//! through the `client_verbs` re-export.

use serde_json::Value;

use crate::identity::session_handle_tier;

/// The tier a token resolves registry row `entry` at: 0 full id, 1 canonical
/// handle, 2 legacy suffix. The row's own id (`fno_id`) addresses it at the
/// full tier; a legacy short or name-valued fno_id keeps resolving through
/// the tiers below. A predecessor id addresses the row at the FULL tier only:
/// succession retired it, so delivery naming A follows the row that now
/// answers as B, while A's retired short/handle forms stay retired.
pub(crate) fn entry_session_tier(entry: &Value, token: &str) -> Option<u8> {
    if is_session_shaped(token) && entry.get("fno_id").and_then(Value::as_str) == Some(token) {
        return Some(0);
    }
    // The row's fno handle (the head of its fno-minted fno_id) addresses it at
    // the same short tier as the harness head, inside the union below.
    let own: Vec<&str> = ["harness_session_id", "related_session_id"]
        .iter()
        .filter_map(|key| entry.get(*key).and_then(Value::as_str))
        .collect();
    if crate::identity::fno_handle(entry.get("fno_id").and_then(Value::as_str), &own)
        .is_some_and(|handle| handle.eq_ignore_ascii_case(token.trim()))
    {
        return Some(1);
    }
    let session_id = entry.get("harness_session_id").and_then(Value::as_str)?;
    if let Some(tier) = session_handle_tier(token, session_id) {
        return Some(tier);
    }
    // The one optional related id addresses the row at the same tiers as the
    // primary (: both ids stay valid forever).
    if let Some(related) = entry.get("related_session_id").and_then(Value::as_str) {
        if let Some(tier) = session_handle_tier(token, related) {
            return Some(tier);
        }
    }
    entry
        .get("predecessor_session_ids")
        .and_then(Value::as_array)
        .and_then(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .any(|id| session_handle_tier(token, id) == Some(0))
                .then_some(0)
        })
}

/// True for a token worth probing a harness store with -- the Rust mirror of
/// `store_fallback.is_session_shaped`. A plain unknown NAME never probes, so a
/// typo keeps today's refusal instead of paying for three store reads.
pub(crate) fn is_session_shaped(token: &str) -> bool {
    let token = token.trim();
    if let Some(rest) = token.strip_prefix("ses_") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric());
    }
    (token.len() == 8 && token.bytes().all(|b| b.is_ascii_alphanumeric()))
        || crate::resume_wake::is_uuid_shaped(&token.to_ascii_lowercase())
}
