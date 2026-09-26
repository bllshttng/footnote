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

/// One planned stamp: the node, the PR, and the recorded outcome. A stamp
/// with `primary` set rides the node's own primary PR; the rest target one
/// `additional_prs` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrStamp {
    pub(crate) node: String,
    pub(crate) number: i64,
    pub(crate) url: Option<String>,
    pub(crate) merge_status: &'static str,
    pub(crate) primary: bool,
}

/// A GitHub PR url's REST path and its number:
/// `https://github.com/<owner>/<repo>/pull/<n>` gives
/// `repos/<owner>/<repo>/pulls/<n>` with that n. Any other shape falls
/// back to the caller's `{owner}/{repo}` placeholders and entry number.
/// The url is the read's authority, so the stamp names the PR the read
/// actually answered, never a diverging `number` field.
fn rest_path_and_number(url: &str) -> Option<(String, i64)> {
    let normalized = normalize_url(url);
    let rest = normalized.strip_prefix("https://github.com/")?;
    let mut segs = rest.split('/');
    let owner = segs.next()?;
    let repo = segs.next()?;
    let kind = segs.next()?;
    let n = segs.next()?;
    if kind != "pull" {
        return None;
    }
    let n: i64 = n.parse().ok()?;
    Some((format!("repos/{owner}/{repo}/pulls/{n}"), n))
}

/// The primary arm's one read: the REST path from the node's own `pr_url`,
/// else `repos/{owner}/{repo}/pulls/<pr_number>` when the url is blank or
/// missing, else no read at all - a url that does not parse is never
/// re-guessed into a repo. Only a `Merged` answer plans a stamp; `Open`,
/// `Closed` and an unreadable answer leave the node exactly as it was.
fn primary_read(
    entry: &Value,
    cwd: &str,
    read: &mut dyn FnMut(&str, &str) -> Option<PrState>,
) -> Option<PrStamp> {
    let node_id = crate::graph_store::entry_id(entry)?.to_string();
    let pr_url = entry.get("pr_url").and_then(Value::as_str);
    let (path, number, url) = match pr_url.and_then(rest_path_and_number) {
        Some((path, n)) => (path, n, pr_url.map(str::to_string)),
        None => {
            if pr_url.is_some_and(|u| !u.trim().is_empty()) {
                return None;
            }
            let n = entry.get("pr_number").and_then(Value::as_i64)?;
            (format!("repos/{{owner}}/{{repo}}/pulls/{n}"), n, None)
        }
    };
    match read(&path, cwd)? {
        PrState::Merged => Some(PrStamp {
            node: node_id,
            number,
            url,
            merge_status: "merged",
            primary: true,
        }),
        PrState::Open | PrState::Closed => None,
    }
}

/// One GitHub read per unsettled PR on a held node, planned ahead of the
/// settle. A done node carrying an open do row and a `cwd` is visited: a
/// node whose own merge is unrecorded reads its primary once, and a merged
/// node's unsettled extras are each read once. A node with no `cwd` gets no
/// read, an entry with no number gets no path, and `Open` plus an
/// unreadable answer plan nothing.
pub(crate) fn plan_stamps(
    entries: &[Value],
    read: &mut dyn FnMut(&str, &str) -> Option<PrState>,
) -> Vec<PrStamp> {
    let mut stamps = Vec::new();
    let primaries = primary_index(entries);
    for entry in entries {
        let Some(node_id) = crate::graph_store::entry_id(entry) else {
            continue;
        };
        if entry.get("status").and_then(Value::as_str) != Some("done") {
            continue;
        }
        let has_open_do = entry
            .get("sessions")
            .and_then(Value::as_array)
            .is_some_and(|rows| rows.iter().any(crate::graph_store::is_open_do_row));
        if !has_open_do {
            continue;
        }
        let Some(cwd) = entry.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let merge_status = entry.get("merge_status").and_then(Value::as_str);
        if merge_status == Some("merged") {
            // fall through to the extras
        } else if merge_status.is_none() {
            // An out-of-band merge the writers never recorded: the one gap
            // the reaper cannot settle past. One read records it; anything
            // else the read could answer leaves the node held.
            if let Some(stamp) = primary_read(entry, cwd, read) {
                stamps.push(stamp);
            } else {
                continue;
            }
        } else {
            // A recorded failure or closed outcome is a verdict, not a gap.
            continue;
        }
        for extra in entry
            .get("additional_prs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !additional_pr_open(extra, node_id, &primaries) {
                continue;
            }
            let url = extra.get("url").and_then(Value::as_str);
            let from_url = url.and_then(rest_path_and_number);
            let number = match from_url {
                Some((_, n)) => n,
                None => match extra.get("number").and_then(Value::as_i64) {
                    Some(n) => n,
                    None => continue,
                },
            };
            let path = from_url
                .map(|(p, _)| p)
                .unwrap_or_else(|| format!("repos/{{owner}}/{{repo}}/pulls/{number}"));
            let Some(state) = read(&path, cwd) else {
                continue;
            };
            let merge_status = match state {
                PrState::Merged => "merged",
                PrState::Closed => "closed",
                PrState::Open => continue,
            };
            stamps.push(PrStamp {
                node: node_id.to_string(),
                number,
                url: url.map(str::to_string),
                merge_status,
                primary: false,
            });
        }
    }
    stamps
}

/// Write planned stamps into rows in memory, for the dry run. The first
/// matching entry per stamp wins; an already-settled entry is skipped. A
/// primary stamp fills the node's top-level `merge_status` when absent or
/// null; the extras arm is unchanged.
pub(crate) fn apply_stamps(entries: &mut [Value], stamps: &[PrStamp]) {
    for stamp in stamps {
        for entry in entries.iter_mut() {
            if crate::graph_store::entry_id(entry) != Some(stamp.node.as_str()) {
                continue;
            }
            if stamp.primary {
                if matches!(entry.get("merge_status"), None | Some(Value::Null)) {
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert(
                            "merge_status".to_string(),
                            Value::String(stamp.merge_status.to_string()),
                        );
                    }
                }
                break;
            }
            let Some(extras) = entry
                .get_mut("additional_prs")
                .and_then(Value::as_array_mut)
            else {
                break;
            };
            for extra in extras {
                if extra.get("number").and_then(Value::as_i64) != Some(stamp.number) {
                    continue;
                }
                if let Some(want) = &stamp.url {
                    let got = extra.get("url").and_then(Value::as_str).map(normalize_url);
                    if got.as_deref() != Some(normalize_url(want).as_str()) {
                        continue;
                    }
                }
                if matches!(
                    extra.get("merge_status").and_then(Value::as_str),
                    Some("merged") | Some("closed")
                ) {
                    continue;
                }
                if let Some(obj) = extra.as_object_mut() {
                    obj.insert(
                        "merge_status".to_string(),
                        Value::String(stamp.merge_status.to_string()),
                    );
                }
                break;
            }
            break;
        }
    }
}

/// The state-shaped REST read behind the sweep's PR questions: `gh api
/// <path>` in the given cwd. `None` unreadable - an unreadable answer never
/// stamps and never retires. Bounded 30s; the caller caches per
/// `(path, cwd)` for one pass, so steady state pays nothing.
pub(crate) fn gh_pr_state(path: &str, cwd: &str) -> Option<PrState> {
    let out = crate::loopcheck::bounded_read(
        "gh".as_ref(),
        &["api", path],
        std::path::Path::new(cwd),
        "gc-sweep",
        std::time::Duration::from_secs(30),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    if v.get("merged_at").and_then(Value::as_str).is_some() {
        return Some(PrState::Merged);
    }
    match v.get("state").and_then(Value::as_str) {
        Some("open") => Some(PrState::Open),
        Some("closed") => Some(PrState::Closed),
        _ => None,
    }
}

/// The production reader for one settle or dry-run pass: every
/// `(path, cwd)` answer is read once and cached for the pass.
pub(crate) fn gh_pr_state_reader() -> impl FnMut(&str, &str) -> Option<PrState> {
    let mut cache: HashMap<(String, String), Option<PrState>> = HashMap::new();
    move |path: &str, cwd: &str| {
        *cache
            .entry((path.to_string(), cwd.to_string()))
            .or_insert_with(|| gh_pr_state(path, cwd))
    }
}

/// The stamp pass ahead of the settle: one GitHub read per unsettled PR on
/// a held node, one store stamp per answer - the primary's through
/// `primary_pr_stamp`, an extra's through `pull_request_stamp` - each write
/// confirmed on the returned node. Every stamp refusal is named, and its
/// row keeps its hold. An unreadable graph plans nothing here: the settle's
/// own read names the refusal, and both legs share the store.
pub(crate) fn stamp_pass(
    home: &crate::paths::AgentsHome,
    read: &mut dyn FnMut(&str, &str) -> Option<PrState>,
) -> Vec<(String, String)> {
    let mut refusals = Vec::new();
    let store = crate::backlog::api::Store::new(&crate::gc_sweep::graph_path(home));
    let Ok(entries) = crate::backlog::api::rows(&store) else {
        return refusals;
    };
    for stamp in plan_stamps(&entries, read) {
        let kind = if stamp.primary {
            "primary stamp"
        } else {
            "stamp"
        };
        let result = if stamp.primary {
            crate::backlog::api::primary_pr_stamp(
                &store,
                &stamp.node,
                stamp.number,
                stamp.url.as_deref(),
                stamp.merge_status,
            )
        } else {
            crate::backlog::api::pull_request_stamp(
                &store,
                &stamp.node,
                stamp.number,
                stamp.url.as_deref(),
                stamp.merge_status,
            )
        };
        match result {
            Ok(payload) if payload.success => {
                let confirmed = payload.node.as_ref().is_some_and(|node| {
                    if stamp.primary {
                        node.primary_pr.as_ref().is_some_and(|pr| {
                            pr.number == Some(stamp.number)
                                && pr.merge_status.as_deref() == Some(stamp.merge_status)
                        })
                    } else {
                        node.additional_prs.iter().flatten().any(|extra| {
                            extra.number == Some(stamp.number)
                                && extra.merge_status.as_deref() == Some(stamp.merge_status)
                        })
                    }
                });
                if !confirmed {
                    refusals.push((
                        stamp.node.clone(),
                        format!("{kind} refused: returned node lacks the stamp"),
                    ));
                }
            }
            Ok(_) => refusals.push((
                stamp.node.clone(),
                if stamp.primary {
                    "primary stamp refused: no matching unrecorded primary".to_string()
                } else {
                    "stamp refused: no matching unsettled entry".to_string()
                },
            )),
            Err(err) => refusals.push((stamp.node.clone(), format!("{kind} refused: {}", err.0))),
        }
    }
    refusals
}

/// The dry-run settle plan with its stamp plan: reads the store, plans
/// stamps, applies them in memory, and returns the stale rows the stamped
/// graph still yields plus the stamps behind them. Writes nothing.
pub(crate) fn plan_settle(
    home: &crate::paths::AgentsHome,
    read: &mut dyn FnMut(&str, &str) -> Option<PrState>,
) -> (Vec<crate::gc_sweep::StaleDoRow>, Vec<PrStamp>) {
    let store = crate::backlog::api::Store::new(&crate::gc_sweep::graph_path(home));
    match crate::backlog::api::rows(&store) {
        Ok(mut entries) => {
            let stamps = plan_stamps(&entries, read);
            apply_stamps(&mut entries, &stamps);
            (crate::gc_sweep::stale_open_do_rows(&entries), stamps)
        }
        Err(_) => (Vec::new(), Vec::new()),
    }
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

    #[test]
    fn plan_reads_once_per_unsettled_extra_and_stamps_the_answer() {
        let entries = vec![
            node(
                "x-spec",
                "https://github.com/o/r/pull/2042",
                json!("merged"),
            ),
            json!({
                "id": "x-hold", "status": "done", "merge_status": "merged",
                "cwd": "/repo/wt",
                "sessions": [{"phase": "execute", "harness": "claude", "session_id": "s1",
                              "started_at": "2026-09-01T01:00:00Z"}],
                "additional_prs": [
                    {"number": 1523, "url": "https://github.com/o/r/pull/1523"}
                ]
            }),
        ];
        let mut calls: Vec<(String, String)> = Vec::new();
        let mut read = |path: &str, cwd: &str| {
            calls.push((path.to_string(), cwd.to_string()));
            Some(PrState::Merged)
        };
        let stamps = plan_stamps(&entries, &mut read);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps[0].node, "x-hold".to_string());
        assert_eq!(stamps[0].number, 1523);
        assert_eq!(stamps[0].merge_status, "merged");
        assert_eq!(
            stamps[0].url.as_deref(),
            Some("https://github.com/o/r/pull/1523")
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            ("repos/o/r/pulls/1523".to_string(), "/repo/wt".to_string())
        );
    }

    #[test]
    fn plan_skips_node_without_cwd_and_entry_without_number() {
        let entries = vec![
            json!({
                "id": "x-nocwd", "status": "done", "merge_status": "merged",
                "sessions": [{"phase": "execute", "harness": "claude", "session_id": "s1",
                              "started_at": "2026-09-01T01:00:00Z"}],
                "additional_prs": [{"number": 5}]
            }),
            json!({
                "id": "x-nonum", "status": "done", "merge_status": "merged",
                "cwd": "/repo/wt",
                "sessions": [{"phase": "execute", "harness": "claude", "session_id": "s2",
                              "started_at": "2026-09-01T01:00:00Z"}],
                "additional_prs": [{"note": "no number"}]
            }),
        ];
        let mut calls = 0;
        let mut read = |_path: &str, _cwd: &str| {
            calls += 1;
            Some(PrState::Merged)
        };
        let stamps = plan_stamps(&entries, &mut read);
        assert!(stamps.is_empty());
        assert_eq!(calls, 0, "no cwd or no number, no read");
    }

    #[test]
    fn plan_names_the_read_pr_never_a_diverging_number_field() {
        let entries = vec![json!({
            "id": "x-diverge", "status": "done", "merge_status": "merged",
            "cwd": "/repo/wt",
            "sessions": [{"phase": "execute", "harness": "claude", "session_id": "s1",
                          "started_at": "2026-09-01T01:00:00Z"}],
            "additional_prs": [
                {"number": 1523, "url": "https://github.com/o/r/pull/999"}
            ]
        })];
        let mut read = |path: &str, _cwd: &str| {
            assert_eq!(path, "repos/o/r/pulls/999");
            Some(PrState::Merged)
        };
        let stamps = plan_stamps(&entries, &mut read);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps[0].number, 999, "the stamp names the read PR");
    }

    #[test]
    fn plan_stamps_closed_and_lets_open_and_none_go() {
        let held = json!({
            "id": "x-hold", "status": "done", "merge_status": "merged",
            "cwd": "/repo/wt",
            "sessions": [{"phase": "execute", "harness": "claude",
                          "session_id": "s1", "started_at": "2026-09-01T01:00:00Z"}],
            "additional_prs": [
                {"number": 7, "url": "https://github.com/o/r/pull/7"},
                {"number": 8, "url": "https://github.com/o/r/pull/8"},
                {"number": 9, "url": "https://github.com/o/r/pull/9"}
            ]
        });
        let mut answers: HashMap<String, Option<PrState>> = HashMap::new();
        answers.insert("repos/o/r/pulls/7".to_string(), Some(PrState::Closed));
        answers.insert("repos/o/r/pulls/8".to_string(), Some(PrState::Open));
        answers.insert("repos/o/r/pulls/9".to_string(), None);
        let entries = vec![held];
        let mut read = |path: &str, _cwd: &str| answers.get(path).copied().flatten();
        let stamps = plan_stamps(&entries, &mut read);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps[0].number, 7);
        assert_eq!(stamps[0].merge_status, "closed");
    }

    #[test]
    fn apply_writes_only_the_matching_entry_and_skips_settled() {
        let mut entries = vec![json!({
            "id": "x-hold", "status": "done", "merge_status": "merged",
            "additional_prs": [
                {"number": 1522},
                {"number": 1523, "merge_status": "merged"}
            ]
        })];
        let stamps = vec![
            PrStamp {
                node: "x-hold".into(),
                number: 1523,
                url: None,
                merge_status: "merged",
                primary: false,
            },
            PrStamp {
                node: "x-hold".into(),
                number: 1522,
                url: None,
                merge_status: "merged",
                primary: false,
            },
            PrStamp {
                node: "x-absent".into(),
                number: 1,
                url: None,
                merge_status: "merged",
                primary: false,
            },
        ];
        apply_stamps(&mut entries, &stamps);
        let extras = entries[0]["additional_prs"].as_array().unwrap();
        assert_eq!(extras[0]["merge_status"], json!("merged"));
        assert_eq!(extras[1]["merge_status"], json!("merged"));
    }
}
