//! The durable-grant verdict and the merge queue, behind the
//! `authorized-merge` verb's `op` field.
//!
//! The spawner records a `merge_grant` receipt on the worker's `phase: do`
//! graph row, so merge authority outlives the worker that earned it. This
//! module is the ONE reader of that record: `grant-verdict` answers for one
//! PR, `grant-queue` answers which PRs a dispatch lane may execute this
//! tick. Ported arm for arm from `cli/src/fno/pr/_merge_grant.py`, which
//! becomes a `verb_call` transport; status, the merge verb and the pr-watch
//! merge phase read one owner here.
//!
//! Every arm fails closed: absence, malformed receipts, ambiguity, a live
//! claim, a switched-off config, and an unreadable graph or config never
//! grant. Config readers resolve to false on unreadable files, so an
//! unreadable config reads `held` here where the Python original read
//! `unknown` - both refuse, only the state word differs.

use crate::agents_config;
use crate::backlog::api::{self as backlog_api, Store as GraphStore};
use crate::claims::{status as claim_status, ClaimState, ClaimState::*};
use crate::finalize::slug_from_git_remote;
use crate::graph_keeper::node_carries_pr;
use crate::king_board::scope::graph_json_path;
use crate::paths::canonical_repo_root;
use crate::tick_ledger::parse_rfc3339_unix;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The verdict vocabulary. `granted` is the only state that authorizes a
/// merge call.
pub const GRANTED: &str = "granted";
pub const REFUSED: &str = "refused";
pub const HELD: &str = "held";
pub const ABSENT: &str = "absent";
pub const UNKNOWN: &str = "unknown";

/// The live-config arms of the verdict, read once per node after a receipt
/// clears. `cfg` is a closure so config files are read only after a receipt
/// clears, which is the Python order.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub enabled: bool,
    pub grant_dispatch: bool,
    pub floor_block: Option<String>,
}

/// The typed answer for one node+PR. `grant` carries the winning receipt
/// verbatim (only when a receipt was selected); `node_id` and `claim_state`
/// name the scope and the liveness reading the verdict was computed from.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub state: &'static str,
    pub reason: String,
    pub node_id: Option<String>,
    pub claim_state: Option<String>,
    pub grant: Option<Value>,
}

/// The keys a receipt may carry, and exactly those.
const GRANT_KEYS: [&str; 4] = ["approved", "source", "recorded_by", "recorded_at"];

/// Why this receipt is unreadable, or None when it is well-formed. Anything
/// the writer could not have minted reads `unknown`, never a partial answer.
fn malformed_grant_reason(grant: &Value) -> Option<String> {
    let Some(map) = grant.as_object() else {
        return Some("merge_grant is not a mapping".to_string());
    };
    let unknown: Vec<&String> = map
        .keys()
        .filter(|k| !GRANT_KEYS.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        let mut names: Vec<String> = unknown.iter().map(|s| s.to_string()).collect();
        names.sort();
        return Some(format!("merge_grant carries unknown keys: {names:?}"));
    }
    let missing: Vec<String> = GRANT_KEYS
        .iter()
        .filter(|key| !map.contains_key(**key))
        .map(|key| key.to_string())
        .collect();
    if !missing.is_empty() {
        let mut names = missing.clone();
        names.sort();
        return Some(format!("merge_grant is missing keys: {names:?}"));
    }
    if grant.get("approved").and_then(Value::as_bool).is_none() {
        return Some("merge_grant.approved is not a boolean".to_string());
    }
    for key in ["source", "recorded_by", "recorded_at"] {
        let ok = grant
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !ok {
            return Some(format!("merge_grant.{key} is not a non-empty string"));
        }
    }
    // The writer mints exactly the canonical "...Z" shape, and newest-wins
    // orders receipts by RAW string comparison: a non-canonical but valid UTC
    // spelling ("+00:00") would sort arbitrarily against canonical rows. Only
    // the exact canonical form is a receipt.
    let stamp = grant
        .get("recorded_at")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_canonical_stamp(stamp.trim()) {
        return Some(format!(
            "merge_grant.recorded_at is not the canonical UTC stamp the writer mints: {stamp:?}"
        ));
    }
    None
}

/// 20 characters of `YYYY-MM-DDTHH:MM:SSZ` that `parse_rfc3339_unix` accepts.
fn is_canonical_stamp(s: &str) -> bool {
    s.len() == 20
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && s.as_bytes()[10] == b'T'
        && s.as_bytes()[13] == b':'
        && s.as_bytes()[16] == b':'
        && s.ends_with('Z')
        && parse_rfc3339_unix(s).is_some()
}

/// Receipts on the node's `phase: do` rows, in row order. `Err` names the
/// malformed receipt and stops the walk: one unreadable receipt is louder
/// than any answer mined past it.
fn do_row_receipts(node: &Value) -> Result<Vec<&Value>, String> {
    let mut receipts = Vec::new();
    let Some(sessions) = node.get("sessions").and_then(Value::as_array) else {
        return Ok(receipts);
    };
    for row in sessions {
        if !row.is_object() {
            continue;
        }
        if row.get("phase").and_then(Value::as_str) != Some("do") {
            continue;
        }
        let grant = match row.get("merge_grant") {
            None | Some(Value::Null) => continue,
            Some(g) => g,
        };
        if let Some(why) = malformed_grant_reason(grant) {
            return Err(why);
        }
        receipts.push(grant);
    }
    Ok(receipts)
}

/// The durable merge verdict for the node this PR delivers. Ports
/// `_merge_grant.py::resolve_durable_grant` arm for arm over already-read
/// entries: pure decision work, no gh call, so the watcher can afford it per
/// tick. `claim_of` reads one node claim; `cfg` reads the live config.
pub fn verdict_for_pr(
    entries: &[Value],
    pr: i64,
    repo: Option<&str>,
    claim_of: &dyn Fn(&str) -> ClaimState,
    cfg: &dyn Fn() -> LiveConfig,
) -> Verdict {
    // Step 1: exactly one graph-linked node.
    let matches: Vec<&Value> = entries
        .iter()
        .filter(|e| node_carries_pr(e, pr, repo))
        .collect();
    if matches.is_empty() {
        return Verdict {
            state: ABSENT,
            reason: "no graph-linked node carries this PR".to_string(),
            node_id: None,
            claim_state: None,
            grant: None,
        };
    }
    if matches.len() > 1 {
        let ids: Vec<String> = matches
            .iter()
            .filter_map(|e| e.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        return Verdict {
            state: UNKNOWN,
            reason: format!(
                "{} nodes link to this PR ({}); an ambiguous scope never grants",
                ids.len(),
                sorted.join(", ")
            ),
            node_id: None,
            claim_state: None,
            grant: None,
        };
    }
    let node = matches[0];
    let node_id = node
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Step 2: receipts on the do rows.
    let receipts = match do_row_receipts(node) {
        Ok(r) => r,
        Err(why) => {
            return Verdict {
                state: UNKNOWN,
                reason: format!("{why} (node {node_id})"),
                node_id: Some(node_id),
                claim_state: None,
                grant: None,
            };
        }
    };
    if receipts.is_empty() {
        return Verdict {
            state: ABSENT,
            reason: "no do row on the node records a merge grant".to_string(),
            node_id: Some(node_id),
            claim_state: None,
            grant: None,
        };
    }

    // Step 3: newest explicit receipt wins, by recorded_at not row position.
    let newest_stamp = receipts
        .iter()
        .filter_map(|r| r.get("recorded_at").and_then(Value::as_str))
        .max()
        .unwrap_or("");
    let newest: Vec<&Value> = receipts
        .iter()
        .copied()
        .filter(|r| r.get("recorded_at").and_then(Value::as_str) == Some(newest_stamp))
        .collect();
    let approved_flags: Vec<bool> = newest
        .iter()
        .filter_map(|r| r.get("approved").and_then(Value::as_bool))
        .collect();
    let first = approved_flags.first().copied().unwrap_or(false);
    if approved_flags.iter().any(|b| *b != first) {
        return Verdict {
            state: UNKNOWN,
            reason: format!(
                "newest durable grants disagree at {newest_stamp} \
(approved={approved_flags:?}); an ambiguous verdict never grants"
            ),
            node_id: Some(node_id),
            claim_state: None,
            grant: None,
        };
    }
    let receipt = newest[0];
    let node_id = Some(node_id);
    let source = receipt.get("source").and_then(Value::as_str).unwrap_or("");
    if !first {
        return Verdict {
            state: REFUSED,
            reason: format!(
                "newest durable grant records approved=false (source: {source}, \
recorded {newest_stamp})"
            ),
            node_id,
            claim_state: None,
            grant: Some(receipt.clone()),
        };
    }

    // Step 4: only a positively not-live holder transfers execution.
    let claim = claim_of(node_id.as_deref().unwrap_or(""));
    let claim_str = claim.as_str();
    if claim == Corrupted {
        return Verdict {
            state: UNKNOWN,
            reason: "node claim unreadable".to_string(),
            node_id,
            claim_state: Some(claim_str.to_string()),
            grant: Some(receipt.clone()),
        };
    }
    if claim != Free && claim != Stale {
        return Verdict {
            state: HELD,
            reason: format!(
                "node claim is {claim_str}; only a positively not-live holder \
transfers execution"
            ),
            node_id,
            claim_state: Some(claim_str.to_string()),
            grant: Some(receipt.clone()),
        };
    }

    // Step 5: live config still decides. A receipt is a record of what the
    // spawner resolved at dispatch time; the standing switch, the grant leaf
    // and the automerge floor are re-read live, so flipping one revokes every
    // stored receipt without touching the graph. The config readers fail
    // closed to false, so an unreadable config reads held, never granted.
    let cfg = cfg();
    if !cfg.enabled {
        return Verdict {
            state: HELD,
            reason: "receipt recorded an approved dispatch but live config \
resolves auto_merge.enabled=false; the standing switch revokes stored receipts"
                .to_string(),
            node_id,
            claim_state: Some(claim_str.to_string()),
            grant: Some(receipt.clone()),
        };
    }
    if !cfg.grant_dispatch {
        return Verdict {
            state: HELD,
            reason: "live config resolves auto_merge.grant not dispatch; the \
recorded receipt does not widen it"
                .to_string(),
            node_id,
            claim_state: Some(claim_str.to_string()),
            grant: Some(receipt.clone()),
        };
    }
    if let Some(why) = cfg.floor_block {
        return Verdict {
            state: HELD,
            reason: why,
            node_id,
            claim_state: Some(claim_str.to_string()),
            grant: Some(receipt.clone()),
        };
    }

    // Step 6: granted.
    Verdict {
        state: GRANTED,
        reason: format!(
            "newest durable grant approved (source: {source}, recorded \
{newest_stamp}), claim {claim_str}, live config grants dispatch"
        ),
        node_id,
        claim_state: Some(claim_str.to_string()),
        grant: Some(receipt.clone()),
    }
}

/// `owner/repo` from a `https://host/<owner>/<repo>/pull/<n>` URL, None for
/// any other shape.
pub fn repo_slug_from_pr_url(pr_url: &str) -> Option<String> {
    let clean = pr_url.split('?').next().unwrap_or(pr_url);
    let clean = clean.split('#').next().unwrap_or(clean);
    let clean = clean.trim_end_matches('/');
    let (head, tail) = clean.rsplit_once("/pull/")?;
    tail.parse::<i64>().ok()?;
    let mut parts: Vec<&str> = head.split('/').filter(|s| !s.is_empty()).collect();
    let repo = parts.pop()?;
    let owner = parts.pop()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Which PRs a dispatch lane may execute this tick, counted over
/// already-read entries. Pure: `root_of` and `cfg_of` are closures so tests
/// need no filesystem.
pub fn queue_from_entries(
    entries: &[Value],
    claim_of: &dyn Fn(&str) -> ClaimState,
    root_of: &dyn Fn(&Value) -> Option<PathBuf>,
    cfg_of: &dyn Fn(&Path) -> LiveConfig,
) -> Value {
    let mut candidates = 0usize;
    let mut verdicts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut queue: Vec<Value> = Vec::new();
    for entry in entries {
        if !entry.is_object() {
            continue;
        }
        if entry.get("status").and_then(Value::as_str) == Some("superseded") {
            continue;
        }
        let Some(pr) = entry.get("pr_number").and_then(Value::as_i64) else {
            continue;
        };
        if matches!(
            entry.get("merge_status").and_then(Value::as_str),
            Some("merged") | Some("closed")
        ) {
            continue;
        }
        let empty = Vec::new();
        let has_grant = entry
            .get("sessions")
            .and_then(Value::as_array)
            .unwrap_or(&empty)
            .iter()
            .any(|row| {
                row.is_object()
                    && row.get("phase").and_then(Value::as_str) == Some("do")
                    && !matches!(row.get("merge_grant"), None | Some(Value::Null))
            });
        if !has_grant {
            continue;
        }
        candidates += 1;
        let Some(slug) = entry
            .get("pr_url")
            .and_then(Value::as_str)
            .and_then(repo_slug_from_pr_url)
        else {
            *verdicts.entry(UNKNOWN).or_default() += 1;
            continue;
        };
        let Some(root) = root_of(entry) else {
            *verdicts.entry(UNKNOWN).or_default() += 1;
            continue;
        };
        let verdict = verdict_for_pr(entries, pr, Some(&slug), claim_of, &|| cfg_of(&root));
        *verdicts.entry(verdict.state).or_default() += 1;
        if verdict.state == GRANTED {
            let grant = verdict.grant.unwrap_or(Value::Null);
            queue.push(json!({
                "node_id": verdict.node_id,
                "pr": pr,
                "repo_slug": slug,
                "cwd": root,
                "grant": {
                    "source": grant.get("source"),
                    "recorded_by": grant.get("recorded_by"),
                    "recorded_at": grant.get("recorded_at"),
                },
            }));
        }
    }
    let counts = json!({
        GRANTED: verdicts.get(GRANTED).copied().unwrap_or(0),
        HELD: verdicts.get(HELD).copied().unwrap_or(0),
        REFUSED: verdicts.get(REFUSED).copied().unwrap_or(0),
        ABSENT: verdicts.get(ABSENT).copied().unwrap_or(0),
        UNKNOWN: verdicts.get(UNKNOWN).copied().unwrap_or(0),
    });
    json!({"candidates": candidates, "verdicts": counts, "queue": queue})
}

fn live_config(root: &Path) -> LiveConfig {
    LiveConfig {
        enabled: agents_config::auto_merge_enabled(root),
        grant_dispatch: agents_config::auto_merge_grant_dispatches(root),
        floor_block: agents_config::automerge_posture_floor_block_reason(root),
    }
}

/// One `grant-verdict` answer over already-read rows. An `Err` rows read is
/// an unknown verdict naming it - an unread graph never grants.
pub fn verdict_op(rows: Result<Vec<Value>, String>, payload: &Value) -> String {
    let entries = match rows {
        Err(e) => {
            return json!({
                "state": UNKNOWN,
                "reason": format!("graph unreadable, refusing to resolve a grant: {e}"),
                "node_id": Value::Null,
                "claim_state": Value::Null,
                "grant": Value::Null,
            })
            .to_string()
        }
        Ok(entries) => entries,
    };
    let pr = payload.get("pr").and_then(Value::as_i64).unwrap_or(0);
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let root = canonical_repo_root(Path::new(cwd)).unwrap_or_else(|| PathBuf::from(cwd));
    let repo = slug_from_git_remote(&root);
    let verdict = verdict_for_pr(
        &entries,
        pr,
        repo.as_deref(),
        &|k| claim_status(k, None).0,
        &|| live_config(&root),
    );
    json!({
        "state": verdict.state,
        "reason": verdict.reason,
        "node_id": verdict.node_id,
        "claim_state": verdict.claim_state,
        "grant": verdict.grant,
    })
    .to_string()
}

/// One `grant-queue` answer over already-read rows. An `Err` rows read is an
/// error receipt - the caller refuses its tick's merge work, it never
/// guesses a queue.
pub fn queue_op(rows: Result<Vec<Value>, String>) -> String {
    match rows {
        Err(e) => json!({"error": format!("graph unreadable: {e}")}).to_string(),
        Ok(entries) => queue_from_entries(
            &entries,
            &|k| claim_status(k, None).0,
            &|entry: &Value| {
                entry
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|cwd| canonical_repo_root(Path::new(cwd)))
            },
            &live_config,
        )
        .to_string(),
    }
}

/// Dispatch one `grant-` op from an `authorized-merge` payload. Always
/// answers with a JSON receipt; the verb's exit status answers only whether
/// the op RAN.
pub fn run_op(op: &str, payload: &Value) -> String {
    match op {
        "grant-verdict" => {
            let rows = read_rows(payload);
            verdict_op(rows, payload)
        }
        "grant-queue" => queue_op(read_rows(payload)),
        other => json!({"error": format!("unknown op {other}")}).to_string(),
    }
}

fn read_rows(payload: &Value) -> Result<Vec<Value>, String> {
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let graph_path = graph_json_path(Path::new(cwd));
    backlog_api::rows(&GraphStore::new(&graph_path)).map_err(|e| e.0)
}

#[cfg(test)]
mod tests;
