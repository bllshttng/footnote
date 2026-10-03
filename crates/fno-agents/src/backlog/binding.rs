//! PR claim binding: validate and bind a PR's closure claims as one
//! graph-owned mutation, ported from graph/_reconcile.py (`bind_pr_rows`,
//! `PrRowBinding`, `node_cwd_in_repo`). Validation completes before any row
//! changes, so unknown or cross-repository claims cannot leave a partial
//! binding behind.

use serde_json::{json, Value};
use std::collections::BTreeSet;

use super::merge_evidence::node_pr_refs;
use super::node_ref::find_node_index;
use super::pr_link::repo_slug_from_url;
use crate::backlog_ready::node_is_open;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PrRowBinding {
    pub node_id: String,
    /// filled_primary | appended_additional | already_bound | already_done
    /// | released
    pub action: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct PrRowBindResult {
    pub outcome: &'static str,
    pub bindings: Vec<PrRowBinding>,
    pub refusal: Option<String>,
}

impl PrRowBindResult {
    fn refused(refusal: impl Into<String>) -> Self {
        Self {
            outcome: "refused",
            bindings: Vec::new(),
            refusal: Some(refusal.into()),
        }
    }

    /// The ids this bind actually bound (the Python `bound_ids` property).
    pub fn bound_ids(&self) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|b| matches!(b.action, "filled_primary" | "appended_additional"))
            .map(|b| b.node_id.clone())
            .collect()
    }
}

/// Validate and bind every PR claim as one mutation. Callers may run this
/// against a copy for a dry-run or under the graph lock for persistence.
pub(crate) fn bind_pr_rows(
    entries: &mut [Value],
    claimed_ids: &[String],
    pr_number: i64,
    pr_url: Option<&str>,
    repo: Option<&str>,
    rebind: bool,
) -> PrRowBindResult {
    if claimed_ids.is_empty() {
        return PrRowBindResult::refused("no closure claims to bind");
    }
    let our_repo = repo
        .map(str::to_string)
        .or_else(|| repo_slug_from_url(pr_url));
    // Validation pass first: the same rows pass two would find.
    let mut resolved: Vec<usize> = Vec::with_capacity(claimed_ids.len());
    for nid in claimed_ids {
        let Some(index) = find_node_index(entries, nid) else {
            return PrRowBindResult::refused(format!("unknown node: {nid}"));
        };
        if let Some(our) = &our_repo {
            for (number, url) in node_pr_refs(&entries[index]) {
                let Some(existing_repo) = repo_slug_from_url(url.as_deref()) else {
                    return PrRowBindResult::refused(format!(
                        "{nid} already carries a PR #{number} ref with no \
                         resolvable repo; this PR is {our} - refusing an \
                         unverifiable cross-repo claim"
                    ));
                };
                if !existing_repo.eq_ignore_ascii_case(our) {
                    return PrRowBindResult::refused(format!(
                        "{nid} already carries a {existing_repo} PR ref; \
                         this PR is {our} - refusing a cross-repo claim"
                    ));
                }
            }
        } else if !node_pr_refs(&entries[index]).is_empty() {
            return PrRowBindResult::refused(format!(
                "{nid} already carries a PR ref and this PR's repo is \
                 unresolvable - refusing an unscoped claim"
            ));
        }
        resolved.push(index);
    }

    // Mutation pass, claim order kept.
    let claimed: BTreeSet<&str> = claimed_ids.iter().map(String::as_str).collect();
    let mut bindings: Vec<PrRowBinding> = Vec::with_capacity(resolved.len());
    for (nid, index) in claimed_ids.iter().zip(resolved) {
        let node = &mut entries[index];
        // The claim line predates the release: skip it, bind the rest.
        let released_from = node
            .get("released_from")
            .and_then(Value::as_str)
            .map(|r| claimed.contains(r))
            .unwrap_or(false);
        let contained_in = node
            .get("contained_in")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if released_from && !contained_in {
            bindings.push(PrRowBinding {
                node_id: nid.clone(),
                action: "released",
            });
            continue;
        }
        let refs = node_pr_refs(node);
        let action = if refs.iter().any(|(number, _)| *number == pr_number) {
            "already_bound"
        } else if !node_is_open(node) {
            "already_done"
        } else if rebind && node.get("pr_number").map(Value::is_i64).unwrap_or(false) {
            let obj = node.as_object_mut().expect("graph rows are objects");
            let old = json!({
                "number": obj.get("pr_number").cloned().unwrap_or(Value::Null),
                "url": obj.get("pr_url").cloned().unwrap_or(Value::Null),
            });
            let mut additional = vec![old];
            additional.extend(
                obj.get("additional_prs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            obj.insert("additional_prs".into(), Value::Array(additional));
            obj.insert("pr_number".into(), json!(pr_number));
            obj.insert("pr_url".into(), json!(pr_url));
            obj.insert("merge_status".into(), Value::Null);
            "appended_additional"
        } else if !node.get("pr_number").map(Value::is_i64).unwrap_or(false) {
            let obj = node.as_object_mut().expect("graph rows are objects");
            obj.insert("pr_number".into(), json!(pr_number));
            obj.insert("pr_url".into(), json!(pr_url));
            "filled_primary"
        } else {
            let obj = node.as_object_mut().expect("graph rows are objects");
            let mut additional: Vec<Value> = obj
                .get("additional_prs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            additional.push(json!({"number": pr_number, "url": pr_url}));
            obj.insert("additional_prs".into(), Value::Array(additional));
            "appended_additional"
        };
        bindings.push(PrRowBinding {
            node_id: nid.clone(),
            action,
        });
    }

    PrRowBindResult {
        outcome: "bound",
        bindings,
        refusal: None,
    }
}

/// Does `entry`'s own `cwd` sit inside `our_root` (or is it missing)?
///
/// Full contract: docs/architecture/backlog-graph-verb-contracts.md
pub(crate) fn node_cwd_in_repo(entry: &Value, our_root: &str) -> bool {
    let Some(raw_cwd) = entry.get("cwd").and_then(Value::as_str) else {
        return true;
    };
    if raw_cwd.is_empty() {
        return true;
    }
    let expanded = expanduser(raw_cwd);
    let norm = normalize_path(&expanded);
    let root_norm = normalize_path(our_root);
    let prefix = format!("{}/", root_norm.trim_end_matches('/'));
    norm == root_norm || norm.starts_with(&prefix)
}

/// `os.path.expanduser` for the shapes a node cwd carries: a leading `~`
/// or `~/`. An unmatched `~user` stays verbatim, as the Python twin's.
fn expanduser(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// Lexical normalization (`os.path.normpath` for the prefix shapes this
/// comparison sees): `.` and `..` resolved, duplicate slashes collapsed,
/// no filesystem access.
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if path.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(extra: Value) -> Value {
        let mut base = json!({"id": "x-aaaa"});
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    #[test]
    fn empty_claims_refuse() {
        let mut entries = vec![node(json!({}))];
        let result = bind_pr_rows(&mut entries, &[], 7, None, None, false);
        assert_eq!(result.outcome, "refused");
        assert_eq!(result.refusal.as_deref(), Some("no closure claims to bind"));
    }

    #[test]
    fn an_unknown_claim_refuses_before_any_row_changes() {
        let mut entries = vec![node(json!({}))];
        let claimed = vec!["x-aaaa".to_string(), "x-missing".to_string()];
        let result = bind_pr_rows(&mut entries, &claimed, 7, None, None, false);
        assert_eq!(result.outcome, "refused");
        assert_eq!(result.refusal.as_deref(), Some("unknown node: x-missing"));
        assert!(entries[0].get("pr_number").is_none(), "no partial binding");
    }

    #[test]
    fn a_cross_repo_claim_refuses() {
        let mut entries = vec![node(json!({
            "pr_number": 3,
            "pr_url": "https://github.com/other/repo/pull/3",
        }))];
        let claimed = vec!["x-aaaa".to_string()];
        let result = bind_pr_rows(&mut entries, &claimed, 7, None, Some("o/r"), false);
        assert_eq!(result.outcome, "refused");
        assert!(result.refusal.unwrap().contains("cross-repo claim"));
    }

    #[test]
    fn a_primary_fill_and_an_additional_append_bind_together() {
        let mut entries = vec![
            node(json!({"id": "x-aaaa"})),
            node(
                json!({"id": "x-bbbb", "pr_number": 2, "pr_url": "https://github.com/o/r/pull/2"}),
            ),
        ];
        let claimed = vec!["x-aaaa".to_string(), "x-bbbb".to_string()];
        let result = bind_pr_rows(
            &mut entries,
            &claimed,
            7,
            Some("https://github.com/o/r/pull/7"),
            Some("o/r"),
            false,
        );
        assert_eq!(result.outcome, "bound", "{:?}", result.refusal);
        assert_eq!(result.bindings[0].action, "filled_primary");
        assert_eq!(result.bindings[1].action, "appended_additional");
        assert_eq!(result.bound_ids(), vec!["x-aaaa", "x-bbbb"]);
        assert_eq!(entries[0]["pr_number"], 7);
        let additional = &entries[1]["additional_prs"];
        assert_eq!(additional[0]["number"], 7);
        // The pre-existing primary stays put.
        assert_eq!(entries[1]["pr_number"], 2);
    }

    #[test]
    fn rebind_moves_the_primary_into_the_additional_list() {
        let mut entries = vec![node(json!({
            "pr_number": 2,
            "pr_url": "https://github.com/o/r/pull/2",
            "merge_status": "MERGED",
        }))];
        let claimed = vec!["x-aaaa".to_string()];
        let result = bind_pr_rows(
            &mut entries,
            &claimed,
            9,
            Some("https://github.com/o/r/pull/9"),
            Some("o/r"),
            true,
        );
        assert_eq!(result.outcome, "bound");
        assert_eq!(result.bindings[0].action, "appended_additional");
        assert_eq!(entries[0]["pr_number"], 9);
        assert!(entries[0]["merge_status"].is_null());
        assert_eq!(entries[0]["additional_prs"][0]["number"], 2);
    }

    #[test]
    fn an_already_bound_or_done_claim_binds_nothing() {
        let mut entries = vec![
            node(json!({"pr_number": 7})),
            node(json!({"completed_at": "2026-10-01T00:00:00Z"})),
        ];
        let claimed = vec!["x-aaaa".to_string(), "x-bbbb".to_string()];
        let result = bind_pr_rows(&mut entries, &claimed, 7, None, Some("o/r"), false);
        assert_eq!(result.outcome, "bound");
        assert_eq!(result.bindings[0].action, "already_bound");
        assert_eq!(result.bindings[1].action, "already_done");
        assert!(result.bound_ids().is_empty());
    }

    #[test]
    fn a_released_claim_line_skips_the_bind() {
        let mut entries = vec![node(json!({"id": "x-aaaa", "released_from": "x-bbbb"}))];
        let claimed = vec!["x-aaaa".to_string(), "x-bbbb".to_string()];
        let result = bind_pr_rows(&mut entries, &claimed, 7, None, Some("o/r"), false);
        assert_eq!(result.outcome, "bound");
        assert_eq!(result.bindings[0].action, "released");
        assert!(entries[0].get("pr_number").is_none());
    }

    #[test]
    fn cwd_inside_outside_and_missing() {
        let root = "/repo/wt";
        assert!(
            node_cwd_in_repo(&node(json!({})), root),
            "missing cwd is in"
        );
        assert!(node_cwd_in_repo(
            &node(json!({"cwd": "/repo/wt/sub/deep"})),
            root
        ));
        assert!(node_cwd_in_repo(&node(json!({"cwd": "/repo/wt"})), root));
        assert!(!node_cwd_in_repo(
            &node(json!({"cwd": "/repo/wt-other"})),
            root
        ));
        assert!(!node_cwd_in_repo(&node(json!({"cwd": "/elsewhere"})), root));
        assert!(node_cwd_in_repo(
            &node(json!({"cwd": "/repo/wt/../wt/x"})),
            root
        ));
    }
}
