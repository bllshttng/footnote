//! Node-reference plumbing shared by the backlog write verbs: the id-shape
//! gate, the lookup tiers (exact id, `ab-` short prefix), and the
//! archived-versus-absent refusal. Moved here from `rank_cli` (id gates,
//! PR two) and `get_cli` (resolve tiers) so `update` and `add` reuse one
//! implementation instead of a third copy.

use serde_json::Value;

use super::settings;

/// `[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}` fullmatch.
pub fn is_wellformed_node_id(s: &str) -> bool {
    let Some((prefix, suffix)) = s.split_once('-') else {
        return false;
    };
    let mut chars = prefix.chars();
    let valid_prefix = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.count() <= 7
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let valid_suffix = (4..=8).contains(&suffix.len())
        && suffix
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    valid_prefix && valid_suffix
}

/// Well-formed, or it opens with the configured prefix or the legacy `ab-`.
pub fn has_node_id_prefix(s: &str) -> bool {
    if is_wellformed_node_id(s) {
        return true;
    }
    s.starts_with(settings::node_id_prefix().as_str()) || s.starts_with("ab-")
}

/// Deterministic tiers 1 to 3, exact only: exact id, exact slug
/// (case insensitive), bare 4 to 8 lowercase hex re-prefixed by the
/// configured prefix then the legacy `ab-`.
pub fn resolve_tiers<'a>(entries: &'a [Value], query: &str) -> Option<&'a Value> {
    for e in entries {
        if e.get("id").and_then(Value::as_str) == Some(query) {
            return Some(e);
        }
    }
    let q_lc = query.to_lowercase();
    for e in entries {
        let slug = e.get("slug").and_then(Value::as_str);
        if slug.is_some_and(|s| !s.is_empty()) && slug == Some(q_lc.as_str()) {
            return Some(e);
        }
    }
    let bare_hex = query.len() >= 4
        && query.len() <= 8
        && query
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if bare_hex {
        let mut prefixes = vec![settings::node_id_prefix()];
        prefixes.push("ab-".to_string());
        prefixes.dedup();
        for p in &prefixes {
            let cand = format!("{p}{query}");
            if let Some(hit) = entries
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(cand.as_str()))
            {
                return Some(hit);
            }
        }
    }
    None
}

/// The `_find_node` ladder: exact id, then the legacy `ab-` short-prefix
/// fuzzy tier (only for `ab-` ids shorter than the full 11 chars). An
/// ambiguous prefix names the candidates on stderr and reads as a miss, the
/// caller's not-found contract.
pub fn find_node<'a>(entries: &'a [Value], node_id: &str) -> Option<&'a Value> {
    if node_id.starts_with("ab-") && node_id.len() < 11 {
        let candidates: Vec<&Value> = entries
            .iter()
            .filter(|e| {
                e.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with(node_id))
            })
            .collect();
        return match candidates.as_slice() {
            [one] => Some(one),
            [] => None,
            many => {
                let ids: Vec<&str> = many
                    .iter()
                    .filter_map(|e| e.get("id").and_then(Value::as_str))
                    .collect();
                eprintln!(
                    "[graph] ambiguous prefix '{node_id}' matches: {}",
                    ids.join(", ")
                );
                None
            }
        };
    }
    entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(node_id))
}

/// The archived read-through on a working-graph miss: the same tiers over the
/// archived residents, else a `previous_id` hit; stamps `_archived` on the
/// clone it returns.
pub fn archive_hit(entries: &[Value], query: &str) -> Option<Value> {
    if let Some(hit) = resolve_tiers(entries, query) {
        let mut row = hit.clone();
        if let Some(obj) = row.as_object_mut() {
            obj.insert("_archived".to_string(), Value::Bool(true));
        }
        return Some(row);
    }
    for e in entries {
        if e.get("previous_id").and_then(Value::as_str) == Some(query) {
            let mut row = e.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("_archived".to_string(), Value::Bool(true));
            }
            return Some(row);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wellformed_gate_matches_the_python_grammar() {
        assert!(is_wellformed_node_id("x-aaaa1111"));
        assert!(is_wellformed_node_id("ab-1234"));
        assert!(!is_wellformed_node_id("nope"));
        assert!(!is_wellformed_node_id("x-AAAA1111"));
        assert!(!is_wellformed_node_id("x-123"));
    }

    #[test]
    fn find_node_unique_prefix_resolves_and_ambiguous_refuses() {
        let entries = vec![
            json!({"id": "ab-12345678"}),
            json!({"id": "ab-12349999"}),
            json!({"id": "x-bbbb2222"}),
        ];
        assert!(find_node(&entries, "ab-12345678").is_some());
        assert!(find_node(&entries, "ab-1234").is_none()); // ambiguous
        assert!(find_node(&entries, "x-bbbb2222").is_some());
        assert!(find_node(&entries, "x-missing").is_none());
    }
}
