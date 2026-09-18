//! Additional-PR openness: whether a done node's `additional_prs` entry
//! still holds its worker. The recorded state answers first; the entry's
//! url is judged against every node's primary PR before anything reads a
//! tracker.

use serde_json::Value;
use std::collections::HashMap;

/// A pull request's live state, as one read reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrState {
    Open,
    Merged,
    Closed,
}

/// Normalized primary `pr_url` -> every node carrying it, with that node's
/// recorded `merge_status`.
pub(crate) type PrimaryIndex = HashMap<String, Vec<(String, Option<String>)>>;

/// Compare urls the way merges repeat them: case-insensitive, whitespace
/// trimmed, at most one trailing slash.
pub(crate) fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    let stripped = trimmed.strip_suffix('/').unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
}

/// Map every node's primary `pr_url` to the nodes carrying it.
pub(crate) fn primary_index(entries: &[Value]) -> PrimaryIndex {
    let mut index: PrimaryIndex = HashMap::new();
    for entry in entries {
        let Some(node_id) = crate::graph_store::entry_id(entry) else {
            continue;
        };
        let Some(url) = entry.get("pr_url").and_then(Value::as_str) else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        index.entry(normalize_url(url)).or_default().push((
            node_id.to_string(),
            entry
                .get("merge_status")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
    }
    index
}

/// Whether an `additional_prs` entry still holds its node's worker (`true`
/// is open, the fail-closed direction). Rules, first match settles:
///
/// 1. the entry's own `merge_status` reads `merged` or `closed`;
/// 2. its `url` is the primary of a node whose `merge_status` reads
///    `merged` - the node itself may be the holder;
/// 3. its `url` is the primary of a node OTHER than `node_id` - that node's
///    own worker holds on the PR, so this node's worker does not wait for
///    it.
///
/// An entry with no `url` matches neither rule 2 nor rule 3. Nothing here
/// queries a live tracker: recording the state at merge time is the merge
/// verb's job, and the sweep's settle pass reads one PR only for an entry
/// these rules cannot settle.
pub(crate) fn additional_pr_open(extra: &Value, node_id: &str, primaries: &PrimaryIndex) -> bool {
    if matches!(
        extra.get("merge_status").and_then(Value::as_str),
        Some("merged") | Some("closed")
    ) {
        return false;
    }
    let Some(url) = extra.get("url").and_then(Value::as_str) else {
        return true;
    };
    let Some(holders) = primaries.get(&normalize_url(url)) else {
        return true;
    };
    !holders.iter().any(|(holder, merge_status)| {
        merge_status.as_deref() == Some("merged") || holder != node_id
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, pr_url: &str, merge_status: Value) -> Value {
        json!({"id": id, "pr_url": pr_url, "merge_status": merge_status})
    }

    #[test]
    fn own_stamp_of_merged_or_closed_settles() {
        let primaries = PrimaryIndex::new();
        for state in ["merged", "closed"] {
            let extra = json!({"number": 7, "merge_status": state});
            assert!(!additional_pr_open(&extra, "x-a", &primaries), "{state}");
        }
    }

    #[test]
    fn no_stamp_and_no_url_is_open() {
        let primaries = PrimaryIndex::new();
        assert!(additional_pr_open(&json!({"number": 7}), "x-a", &primaries));
        assert!(additional_pr_open(
            &json!({"number": 7, "url": ""}),
            "x-a",
            &primaries
        ));
    }

    #[test]
    fn primary_of_a_merged_node_settles_even_itself() {
        let entries = vec![
            node("x-a", "https://github.com/o/r/pull/42", json!("merged")),
            node("x-b", "https://github.com/o/r/pull/43", json!(null)),
        ];
        let primaries = primary_index(&entries);
        let own_primary = json!({"number": 42, "url": "https://github.com/o/r/pull/42"});
        assert!(!additional_pr_open(&own_primary, "x-a", &primaries));
        let merged_primary = json!({"number": 42, "url": "HTTPS://GitHub.com/o/r/pull/42/"});
        assert!(!additional_pr_open(&merged_primary, "x-b", &primaries));
    }

    #[test]
    fn primary_of_another_node_settles_without_a_merge() {
        let entries = vec![node("x-b", "https://github.com/o/r/pull/43", Value::Null)];
        let primaries = primary_index(&entries);
        let extra = json!({"number": 43, "url": "https://github.com/o/r/pull/43"});
        assert!(!additional_pr_open(&extra, "x-a", &primaries));
    }

    #[test]
    fn url_matching_ignores_case_and_one_trailing_slash() {
        let entries = vec![node(
            "x-b",
            "https://github.com/Org/Repo/pull/43/",
            Value::Null,
        )];
        let primaries = primary_index(&entries);
        let extra = json!({"number": 43, "url": "https://github.com/org/repo/pull/43"});
        assert!(!additional_pr_open(&extra, "x-a", &primaries));
    }

    #[test]
    fn url_owned_by_no_node_stays_open() {
        let entries = vec![node("x-b", "https://github.com/o/r/pull/43", Value::Null)];
        let primaries = primary_index(&entries);
        let extra = json!({"number": 99, "url": "https://github.com/o/r/pull/99"});
        assert!(additional_pr_open(&extra, "x-a", &primaries));
    }

    #[test]
    fn index_skips_nodes_without_a_url_and_blank_urls() {
        let entries = vec![
            json!({"id": "x-nourl", "merge_status": "merged"}),
            json!({"id": "x-blank", "pr_url": "  "}),
            node("x-a", "https://github.com/o/r/pull/42", json!("merged")),
        ];
        let primaries = primary_index(&entries);
        assert_eq!(primaries.len(), 1);
        assert_eq!(
            primaries["https://github.com/o/r/pull/42"][0].0,
            "x-a".to_string()
        );
    }

    #[test]
    fn normalize_trims_case_and_one_slash_only() {
        assert_eq!(normalize_url(" https://X.io/R/ "), "https://x.io/r");
        assert_eq!(normalize_url("https://x.io/r//"), "https://x.io/r/");
    }
}
