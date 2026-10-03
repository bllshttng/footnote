//! Node-reference plumbing shared by the backlog write verbs: the id-shape
//! gate, the lookup tiers (exact id, `ab-` short prefix), and the
//! archived-versus-absent refusal. Moved here from `rank_cli` (id gates,
//! PR two) and `get_cli` (resolve tiers) so `update` and `add` reuse one
//! implementation instead of a third copy.

use serde_json::Value;

use super::settings;

/// `[a-z][a-z0-9]{0,7}-?[0-9a-f]{4,8}` fullmatch - the dash is optional so
/// the dash-less ids the minter briefly minted before 2026-09-27 stay
/// first-class. The graph, not this shape, is the identity check.
pub fn is_wellformed_node_id(s: &str) -> bool {
    if let Some((prefix, suffix)) = s.split_once('-') {
        return valid_prefix(prefix) && valid_hex_suffix(suffix);
    }
    // No dash: try every prefix/hex split. get() (not split_at) keeps a
    // multibyte input a refusal instead of a panic on a non-char boundary.
    let bytes = s.as_bytes();
    (4..=8).any(|tail| {
        bytes.len() > tail && {
            let (Some(head), Some(hex_tail)) = (s.get(..s.len() - tail), s.get(s.len() - tail..))
            else {
                return false;
            };
            valid_prefix(head) && valid_hex_suffix(hex_tail)
        }
    })
}

fn valid_prefix(prefix: &str) -> bool {
    let mut chars = prefix.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.count() <= 7
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn valid_hex_suffix(suffix: &str) -> bool {
    (4..=8).contains(&suffix.len())
        && suffix
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The alternate spellings of one node id (`x-bbbb` <-> `xbbbb`): the
/// minter briefly minted dash-less ids, and resolution is format-agnostic.
/// Empty when the query carries no dash to remove or no room to insert one.
pub fn dash_variants(q: &str) -> Vec<String> {
    let mut alts: Vec<String> = Vec::new();
    if let Some(pos) = q.find('-') {
        alts.push(format!("{}{}", &q[..pos], &q[pos + 1..]));
    } else if q.len() >= 5 {
        for i in 1..q.len().min(9) {
            alts.push(format!("{}-{}", &q[..i], &q[i..]));
        }
    }
    alts
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
    // A dash-leading token is a leaked flag, never a node id (the grammar is
    // lowercase-led): refuse before any graph lookup.
    if query.starts_with('-') {
        return None;
    }
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
    // Dash variants (`x-bbbb` <-> `xbbbb`): the graph confirms the id, not
    // the spelling. Still exact-match only.
    for alt in dash_variants(&q_lc) {
        if let Some(hit) = entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(alt.as_str()))
        {
            return Some(hit);
        }
    }
    None
}

/// The `_find_node` ladder: exact id, then the legacy `ab-` short-prefix
/// fuzzy tier (only for `ab-` ids shorter than the full 11 chars). An
/// ambiguous prefix names the candidates on stderr and reads as a miss, the
/// caller's not-found contract.
pub fn find_node<'a>(entries: &'a [Value], node_id: &str) -> Option<&'a Value> {
    entries.get(find_node_index(entries, node_id)?)
}

/// The find_node tiers over indices, for callers that must mutate the row
/// they resolved (the same node the immutable view would have returned).
pub fn find_node_index(entries: &[Value], node_id: &str) -> Option<usize> {
    // Same leaked-flag refusal as resolve_tiers: a dash-leading token never
    // reaches the graph.
    if node_id.starts_with('-') {
        return None;
    }
    if node_id.starts_with("ab-") && node_id.len() < 11 {
        let candidates: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with(node_id))
            })
            .map(|(i, _)| i)
            .collect();
        return match candidates.as_slice() {
            [one] => Some(*one),
            [] => None,
            many => {
                let ids: Vec<&str> = many
                    .iter()
                    .filter_map(|i| entries[*i].get("id").and_then(Value::as_str))
                    .collect();
                eprintln!(
                    "[graph] ambiguous prefix '{node_id}' matches: {}",
                    ids.join(", ")
                );
                None
            }
        };
    }
    // Dash variants (`x-bbbb` <-> `xbbbb`), still exact-match only: the graph
    // confirms the id, not the spelling. Exact spelling wins first.
    if let Some(exact) = entries
        .iter()
        .position(|e| e.get("id").and_then(Value::as_str) == Some(node_id))
    {
        return Some(exact);
    }
    for alt in dash_variants(&node_id.to_lowercase()) {
        if let Some(hit) = entries
            .iter()
            .position(|e| e.get("id").and_then(Value::as_str) == Some(alt.as_str()))
        {
            return Some(hit);
        }
    }
    None
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

/// Whether setting `node.parent = proposed_parent_id` forms a cycle: the
/// proposed parent's ancestor chain already reaches the node.
pub fn would_create_cycle(entries: &[Value], node_id: &str, proposed_parent_id: &str) -> bool {
    if proposed_parent_id == node_id {
        return true;
    }
    if entries.is_empty() {
        return false;
    }
    let id_to_entry: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    seen.insert(node_id);
    let mut current = Some(proposed_parent_id);
    while let Some(cur) = current {
        if cur == node_id || !seen.insert(cur) {
            return true;
        }
        let Some(ancestor) = id_to_entry.get(cur) else {
            return false;
        };
        current = ancestor.get("parent").and_then(Value::as_str);
    }
    false
}

/// Whether parenting `node` under `parent_node` breaks the two-level epic
/// cap: both epics, and either the parent is already nested or the node owns
/// an epic subtree.
pub fn would_exceed_epic_depth(entries: &[Value], node: &Value, parent_node: &Value) -> bool {
    if node.get("type").and_then(Value::as_str) != Some("epic")
        || parent_node.get("type").and_then(Value::as_str) != Some("epic")
    {
        return false;
    }
    let id_to_entry: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();
    // (a) walk UP from the parent: any epic ancestor means it is nested.
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut current = parent_node.get("parent").and_then(Value::as_str);
    while let Some(cur) = current {
        if !seen.insert(cur.to_string()) {
            break;
        }
        let Some(ancestor) = id_to_entry.get(cur) else {
            break;
        };
        if ancestor.get("type").and_then(Value::as_str) == Some("epic") {
            return true;
        }
        current = ancestor.get("parent").and_then(Value::as_str);
    }
    // (b) walk DOWN from the node: an epic descendant means the node is
    // itself a mission; nesting it under an epic exceeds the cap.
    if let Some(nid) = node.get("id").and_then(Value::as_str) {
        for desc_id in crate::backlog_ready::descendants_of(entries, nid) {
            if let Some(desc) = id_to_entry.get(desc_id.as_str()) {
                if desc.get("type").and_then(Value::as_str) == Some("epic") {
                    return true;
                }
            }
        }
    }
    false
}

/// The blocker-ladder validation: every id exists, none is the node itself,
/// and no (transitive) blocked_by chain already reaches the node.
pub fn validate_blockers(
    blockers: &[String],
    entries: &[Value],
    task_id: &str,
) -> Result<(), (String, i32)> {
    let id_to_entry: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();
    for bid in blockers {
        if !id_to_entry.contains_key(bid.as_str()) {
            return Err((format!("Error: unknown blocker id '{bid}'"), 2));
        }
        if bid == task_id {
            return Err((format!("Error: node cannot block itself ({task_id})"), 2));
        }
        let mut visited: std::collections::HashSet<String> = Default::default();
        let mut stack = vec![bid.clone()];
        while let Some(curr) = stack.pop() {
            if curr == task_id {
                return Err((
                    format!("Error: cycle detected - {bid} (transitively) depends on {task_id}"),
                    2,
                ));
            }
            if !visited.insert(curr.clone()) {
                continue;
            }
            if let Some(entry) = id_to_entry.get(curr.as_str()) {
                if let Some(blocked) = entry.get("blocked_by").and_then(Value::as_array) {
                    stack.extend(
                        blocked
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(str::to_string),
                    );
                }
            }
        }
    }
    Ok(())
}

/// `_resolve_asserted_id`: a caller-asserted reference must resolve through
/// the exact tiers, and never name the node itself.
pub fn resolve_asserted_id(
    token: &str,
    entries: &[Value],
    flag: &str,
    self_id: Option<&str>,
) -> Result<String, (String, i32)> {
    match resolve_tiers(entries, token) {
        Some(hit) => {
            let resolved = hit
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(self_id) = self_id {
                if resolved == self_id {
                    return Err((
                        format!("Error: {flag} cannot reference the node itself ({self_id})"),
                        1,
                    ));
                }
            }
            Ok(resolved)
        }
        None => Err((
            format!("Error: {flag} '{token}' does not resolve to a node"),
            1,
        )),
    }
}

/// The `(pr_number, pr_url)` refs one node carries: the primary pair first,
/// then every `additional_prs` entry, deduplicated by number.
pub(crate) fn node_pr_refs(node: &Value) -> Vec<(i64, Option<String>)> {
    let mut refs: Vec<(i64, Option<String>)> = Vec::new();
    let mut seen: std::collections::HashSet<i64> = Default::default();
    if let Some(n) = node.get("pr_number").and_then(Value::as_i64) {
        refs.push((
            n,
            node.get("pr_url")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
        seen.insert(n);
    }
    if let Some(extras) = node.get("additional_prs").and_then(Value::as_array) {
        for extra in extras {
            let Some(obj) = extra.as_object() else {
                continue;
            };
            if let Some(n) = obj.get("number").and_then(Value::as_i64) {
                if seen.insert(n) {
                    refs.push((
                        n,
                        obj.get("url").and_then(Value::as_str).map(str::to_string),
                    ));
                }
            }
        }
    }
    refs
}

/// Un-contain `node_id`, dropping the PR refs inherited from its owner so the
/// owner's merge cannot close it. The python twin lives in `graph/_contain.py`.
pub fn release_contained(entries: &mut [Value], node_id: &str) -> Result<(), String> {
    let idx = entries
        .iter()
        .position(|e| e.get("id").and_then(Value::as_str) == Some(node_id))
        .ok_or_else(|| format!("no node resolves to '{node_id}'"))?;
    let Some(owner_id) = entries[idx]
        .get("contained_in")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    entries[idx]
        .as_object_mut()
        .expect("row is an object")
        .remove("contained_in");
    let owner = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(owner_id.as_str()))
        .cloned();
    let keys: std::collections::HashSet<(i64, String)> = owner
        .as_ref()
        .map(|o| {
            node_pr_refs(o)
                .into_iter()
                .map(|(n, u)| {
                    let slug = crate::backlog::pr_link::repo_slug_from_url(u.as_deref())
                        .unwrap_or_default()
                        .to_lowercase();
                    (n, slug)
                })
                .collect()
        })
        .unwrap_or_default();
    let own_number = entries[idx].get("pr_number").and_then(Value::as_i64);
    let own_slug = crate::backlog::pr_link::repo_slug_from_url(
        entries[idx].get("pr_url").and_then(Value::as_str),
    )
    .map(|s| s.to_lowercase());
    let own_in_keys = own_number
        .map(|n| keys.contains(&(n, own_slug.clone().unwrap_or_default())))
        .unwrap_or(false);
    let obj = entries[idx].as_object_mut().expect("row is an object");
    if own_in_keys {
        obj.insert("pr_number".into(), Value::Null);
        obj.insert("pr_url".into(), Value::Null);
        obj.insert("merge_status".into(), Value::Null);
    }
    if !keys.is_empty() {
        if let Some(extra) = obj.get("additional_prs").cloned() {
            let filtered = match extra {
                Value::Array(items) => Value::Array(
                    items
                        .into_iter()
                        .filter(|item| {
                            let Some(map) = item.as_object() else {
                                return false; // python drops non-dict rows here
                            };
                            let number: Option<i64> = map.get("number").and_then(Value::as_i64);
                            let slug: Option<String> = crate::backlog::pr_link::repo_slug_from_url(
                                map.get("url").and_then(Value::as_str),
                            )
                            .map(|s: String| s.to_lowercase());
                            match (number, slug) {
                                (Some(n), Some(s)) => !keys.contains(&(n, s)),
                                _ => true,
                            }
                        })
                        .collect(),
                ),
                other => other,
            };
            obj.insert("additional_prs".into(), filtered);
        }
    }
    obj.insert("released_from".into(), Value::String(owner_id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wellformed_gate_matches_python_and_compact_legacy_grammar() {
        assert!(is_wellformed_node_id("x-aaaa1111"));
        assert!(is_wellformed_node_id("xd863"));
        assert!(is_wellformed_node_id("ab-1234"));
        assert!(!is_wellformed_node_id("nope"));
        assert!(!is_wellformed_node_id("x-AAAA1111"));
        assert!(!is_wellformed_node_id("x-123"));
        assert!(!is_wellformed_node_id("xg863"));
    }

    #[test]
    fn wellformed_gate_accepts_the_dash_less_shape() {
        assert!(is_wellformed_node_id("xbbbb"));
        assert!(is_wellformed_node_id("xaaaa"));
        assert!(!is_wellformed_node_id("x123"));
        assert!(!is_wellformed_node_id("xB299"));
        assert!(!is_wellformed_node_id("-6a95"));
        assert!(!is_wellformed_node_id("x"));
        // A multibyte char at a would-be split point refuses, never panics.
        assert!(!is_wellformed_node_id("x\u{e9}f9c1d2"));
    }

    #[test]
    fn resolvers_refuse_a_flag_shaped_token_before_the_graph() {
        // A leaked flag (`claim acquire got id --strict`) must never reach the
        // lookup tiers, exact or aliased.
        let entries = vec![json!({"id": "x-aaaa", "slug": "a"})];
        assert!(resolve_tiers(&entries, "--strict").is_none());
        assert!(resolve_tiers(&entries, "-strict").is_none());
        assert!(find_node(&entries, "--strict").is_none());
        assert!(find_node(&entries, "-x-aaaa").is_none());
        // The dash-shaped alias of a real id is a flag leak too, not a spelling.
        assert!(resolve_tiers(&entries, "-aaaa").is_none());
    }

    #[test]
    fn resolve_tiers_aliases_dash_shapes_both_ways() {
        let compact = vec![json!({"id": "xbbbb", "slug": "xbbbb"})];
        assert!(resolve_tiers(&compact, "xbbbb").is_some());
        assert!(resolve_tiers(&compact, "x-bbbb").is_some());
        let dashed = vec![json!({"id": "x-aaaa", "slug": "a"})];
        assert!(resolve_tiers(&dashed, "x-aaaa").is_some());
        assert!(resolve_tiers(&dashed, "xaaaa").is_some());
        assert!(resolve_tiers(&dashed, "x-missing").is_none());
    }

    #[test]
    fn find_node_aliases_the_dash_variant_after_exact() {
        let entries = vec![json!({"id": "xbbbb"}), json!({"id": "x-aaaa"})];
        // Exact spelling wins first...
        assert!(find_node(&entries, "x-aaaa").is_some());
        // ...then the dash variant aliases both ways.
        assert!(find_node(&entries, "x-bbbb").is_some());
        assert!(find_node(&entries, "xaaaa").is_some());
        assert!(find_node(&entries, "x-missing").is_none());
    }

    #[test]
    fn find_node_unique_prefix_resolves_and_ambiguous_refuses() {
        let entries = vec![
            json!({"id": "ab-1a2b3c4d"}),
            json!({"id": "ab-1234abcd"}),
            json!({"id": "x-bbbb2222"}),
        ];
        assert!(find_node(&entries, "ab-1a2b3c4d").is_some());
        // Both stored ids carry the bare `ab-` family prefix, so the fuzzy
        // tier names two candidates and reads as a miss.
        assert!(find_node(&entries, "ab-").is_none());
        assert!(find_node(&entries, "x-bbbb2222").is_some());
        assert!(find_node(&entries, "x-missing").is_none());
    }

    #[test]
    fn cycle_and_depth_guards_walk_the_tree() {
        let entries = vec![
            json!({"id": "x-eeee5555", "type": "epic", "parent": Value::Null}),
            json!({"id": "x-9999aaaa", "type": "epic", "parent": "x-eeee5555"}),
            json!({"id": "x-abcd1234", "type": "feature", "parent": "x-9999aaaa"}),
        ];
        // Parenting the top epic under its own child closes a cycle.
        assert!(would_create_cycle(&entries, "x-eeee5555", "x-9999aaaa"));
        // Both epics with the parent already nested exceeds the cap.
        assert!(would_exceed_epic_depth(&entries, &entries[0], &entries[1]));
        // A feature under an epic is fine.
        assert!(!would_exceed_epic_depth(&entries, &entries[2], &entries[1]));
    }

    #[test]
    fn blocker_validation_refuses_unknown_self_and_cycle() {
        let entries = vec![
            json!({"id": "x-aaaa1111"}),
            json!({"id": "x-bbbb2222", "blocked_by": ["x-aaaa1111"]}),
        ];
        assert!(validate_blockers(&["x-11118888".into()], &entries, "x-aaaa1111").is_err());
        assert!(validate_blockers(&["x-aaaa1111".into()], &entries, "x-aaaa1111").is_err());
        // x-bbbb2222 transitively depends on x-aaaa1111.
        assert!(validate_blockers(&["x-bbbb2222".into()], &entries, "x-aaaa1111").is_err());
        assert!(validate_blockers(&["x-bbbb2222".into()], &entries, "x-dddd4444").is_ok());
    }
}
