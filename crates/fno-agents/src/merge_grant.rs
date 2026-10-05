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
//!
//! The queue drops `superseded` and `done` nodes, reads each checkout's
//! repo root and live config once, and rotates its head by the caller's
//! tick index, so a slow head never starves the tail.

use crate::agents_config;
use crate::backlog::api as backlog_api;
use crate::claims::{status as claim_status, ClaimState, ClaimState::*};
use crate::finalize::slug_from_git_remote;
use crate::graph_keeper::node_carries_pr;
use crate::org_board::scope::graph_json_path;
use crate::paths::canonical_repo_root;
use crate::tick_ledger::parse_rfc3339_unix;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The verdict vocabulary. `granted` is the only state that authorizes a
/// merge call.
pub const GRANTED: &str = "granted";
pub const REFUSED: &str = "refused";
pub const HELD: &str = "held";
pub const ABSENT: &str = "absent";
pub const UNKNOWN: &str = "unknown";

/// The law-lane subject prefix of the head-scoped operator merge grant.
/// The full subject is `{MERGE_GRANT_SUBJECT}:<owner/repo>#<pr>@<40-hex head>`,
/// so a push invalidates the grant by construction: the new head's subject
/// has no rows.
pub const MERGE_GRANT_SUBJECT: &str = "merge-grant";

/// The one decision value that counts as an affirmative grant, mirroring the
/// waiver's exact-match polarity (`coverage_status::WAIVER_DECISION`): row
/// existence carries none, so a note or a denial at the subject reads no.
pub const MERGE_GRANT_DECISION: &str = "merge authorized for this head";

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
        if row.get("phase").and_then(Value::as_str) != Some("execute") {
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

/// The head-scoped subject one merge grant lives at. Same trust shape as the
/// review-coverage waiver's `scoped_waiver_subject`, over a different action.
pub fn head_grant_subject(repo_slug: &str, pr: i64, head: &str) -> String {
    format!("{MERGE_GRANT_SUBJECT}:{repo_slug}#{pr}@{head}")
}

/// The head-scoped parts of a subject [`head_grant_subject`] mints:
/// `(repo_slug, pr, head)`; `None` for anything else. One parser for the
/// format, so the answer side derives the same subject the gate reads. The
/// head must be the full 40-hex sha the writer mints: a prefix would let one
/// subject answer for heads it does not name.
pub fn parse_head_grant_subject(subject: &str) -> Option<(String, i64, String)> {
    let rest = subject
        .strip_prefix(MERGE_GRANT_SUBJECT)?
        .strip_prefix(':')?;
    let (repo, rest) = rest.split_once('#')?;
    let (pr, head) = rest.split_once('@')?;
    // The slug is whatever the minter held (`bllshttng/footnote` from a
    // remote URL, a bare `footnote` from a short remote), so any non-empty
    // token between the colon and the `#` is the subject's repo half.
    if repo.is_empty() || head.len() != 40 || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let pr: i64 = pr.parse().ok()?;
    (pr > 0).then_some((repo.to_string(), pr, head.to_string()))
}

/// The attended command an operator runs in their own terminal to record the
/// grant. Only that door can carry it: `decide/__init__.py` refuses
/// `--authority operator` from any agent session, so a worker can never mint
/// one, and this string is what a per-run refusal names as its one remedy.
pub fn attended_grant_command(repo_slug: &str, pr: i64, head: &str) -> String {
    format!(
        "fno backlog decide '{}' '{}' --authority operator",
        head_grant_subject(repo_slug, pr, head),
        MERGE_GRANT_DECISION
    )
}

/// The head-grant reading over one `decisions` payload. Same polarity as the
/// durable verdict: only a positively affirmative operator row set grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadGrant {
    /// Every operator row at the subject carries `MERGE_GRANT_DECISION`, so
    /// identical duplicates read granted once (idempotent re-records).
    Granted,
    /// No operator row at the subject. `chat_attested` and unattributed rows
    /// are already filtered out by `operator_law_rows`.
    Absent,
    /// Operator rows disagree, or one carries no readable decision.
    Conflict,
    /// No payload (nonzero exit, dead probe) or a payload without the
    /// `decisions` array. Never reads as either grant or absence.
    Unreadable(String),
}

/// The pure grant reader over an already-read `decisions` stdout.
/// `None` stdout (the read failed) is `Unreadable`, never `Absent`.
pub fn head_grant_status(stdout: Option<&[u8]>) -> HeadGrant {
    let Some(bytes) = stdout else {
        return HeadGrant::Unreadable("the decisions read did not answer".to_string());
    };
    let Some(rows) = crate::loopcheck::coverage_status::operator_law_rows(bytes) else {
        return HeadGrant::Unreadable("malformed decisions payload".to_string());
    };
    if rows.is_empty() {
        return HeadGrant::Absent;
    }
    let missing = rows
        .iter()
        .filter(|r| r.get("decision").and_then(|d| d.as_str()) != Some(MERGE_GRANT_DECISION))
        .count();
    if missing > 0 {
        HeadGrant::Conflict
    } else {
        HeadGrant::Granted
    }
}

/// Which PRs a dispatch lane may execute this tick, counted over
/// already-read entries. Pure: `root_of` and `cfg_of` are closures so tests
/// need no filesystem. `root_of` must depend only on the entry's `cwd`,
/// because roots are memoized by that string.
pub fn queue_from_entries(
    entries: &[Value],
    claim_of: &dyn Fn(&str) -> ClaimState,
    root_of: &dyn Fn(&Value) -> Option<PathBuf>,
    cfg_of: &dyn Fn(&Path) -> LiveConfig,
    rotate: u64,
) -> Value {
    let mut candidates = 0usize;
    let mut verdicts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut roots: HashMap<String, Option<PathBuf>> = HashMap::new();
    let configs: RefCell<HashMap<PathBuf, LiveConfig>> = RefCell::new(HashMap::new());
    let mut queue: Vec<Value> = Vec::new();
    for entry in entries {
        if !entry.is_object() {
            continue;
        }
        if matches!(
            entry.get("status").and_then(Value::as_str),
            Some("superseded") | Some("done")
        ) {
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
                    && row.get("phase").and_then(Value::as_str) == Some("execute")
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
        let cwd_key = entry
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !roots.contains_key(&cwd_key) {
            roots.insert(cwd_key.clone(), root_of(entry));
        }
        let Some(root) = roots.get(&cwd_key).cloned().unwrap_or(None) else {
            *verdicts.entry(UNKNOWN).or_default() += 1;
            continue;
        };
        let verdict = verdict_for_pr(entries, pr, Some(&slug), claim_of, &|| {
            configs
                .borrow_mut()
                .entry(root.clone())
                .or_insert_with(|| cfg_of(&root))
                .clone()
        });
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
    if !queue.is_empty() {
        let k = (rotate % queue.len() as u64) as usize;
        queue.rotate_left(k);
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

// ── the PR's bound target manifest ─────────────────────────────────────────

/// One `git worktree list --porcelain` record.
pub(crate) struct WorktreeEntry {
    pub path: String,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Parse the porcelain listing. Shared with the pr_status worktree probe so
/// both readers match worktrees the same way.
pub(crate) fn parse_worktree_list(text: &str) -> Vec<WorktreeEntry> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(done) = current.take() {
                entries.push(done);
            }
            current = Some(WorktreeEntry {
                path: path.to_string(),
                head: None,
                branch: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(c) = current.as_mut() {
                c.head = Some(head.to_string());
            }
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(c) = current.as_mut() {
                c.branch = Some(branch.to_string());
            }
        }
    }
    if let Some(done) = current.take() {
        entries.push(done);
    }
    entries
}

/// `fnmatch`-lite for the archived-manifest patterns; the only glob
/// metachar the names carry is `*`.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], n) || (!n.is_empty() && inner(p, &n[1..])),
            (Some(a), Some(b)) if a == b => inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

/// The worktrees of `cwd` that exist on disk.
fn worktree_paths(cwd: &Path) -> Vec<PathBuf> {
    let Ok(listed) = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
    else {
        return Vec::new();
    };
    if !listed.status.success() {
        return Vec::new();
    }
    parse_worktree_list(&String::from_utf8_lossy(&listed.stdout))
        .into_iter()
        .filter(|e| Path::new(&e.path).is_dir())
        .map(|e| PathBuf::from(e.path))
        .collect()
}

/// One worktree's bound target manifest, read through the canonical
/// state-path owner (`worktree_space_dir` first, legacy `.fno` fallback) and
/// parsed with the hardened finalize parser, whose trust gate keeps prose
/// inside a spilled `input` scalar from minting a merge posture. A live
/// manifest outranks the supported archived/terminal forms; the newest of
/// those is consulted only when no live manifest exists.
#[derive(Debug)]
pub(crate) struct BoundManifestRead {
    pub node_id: Option<String>,
    pub approved: Option<bool>,
    pub source: Option<String>,
    pub session: Option<String>,
    pub manifest_path: PathBuf,
    pub live: bool,
}

/// The manifest reading for one worktree. `None` = no manifest form lives
/// there, so a manifest-less historical receipt keeps its legacy behavior.
/// `Unreadable` = a manifest exists but cannot be read, which never reads as
/// permission.
pub(crate) enum BoundRead {
    None,
    Read(BoundManifestRead),
    Unreadable(String),
}

/// Read the bound manifest of one worktree: the canonical state-path
/// resolution first (live wins), then the newest archived/terminal form in
/// either the resolved directory or the legacy `.fno` state dir.
pub(crate) fn read_bound_manifest(wt: &Path) -> BoundRead {
    // `resolve` always answers for `target-state`; the None arm is defense in
    // depth, never a second path builder.
    let Some(resolved) = crate::state_path::resolve("target-state", wt) else {
        return BoundRead::None;
    };
    if resolved.exists() && !resolved.is_file() {
        return BoundRead::Unreadable(format!("{} is not a regular file", resolved.display()));
    }
    if resolved.is_file() {
        return match std::fs::read_to_string(&resolved) {
            Ok(content) => parse_bound_content(&content, &resolved, true),
            Err(e) => BoundRead::Unreadable(format!("{}: {e}", resolved.display())),
        };
    }
    // Archived/terminal forms, newest first, from the resolved directory
    // (the space dir) and the legacy `.fno` dir.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = resolved.parent() {
        dirs.push(parent.to_path_buf());
    }
    let legacy = wt.join(".fno");
    if legacy != resolved.parent().unwrap_or(Path::new("")) {
        dirs.push(legacy);
    }
    let mut hits: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        hits.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    glob_match("target-state.terminal.*.md", n)
                        || glob_match("target-state.md.archived.*.md", n)
                })
                .unwrap_or(false)
        }));
    }
    hits.sort();
    hits.dedup();
    if let Some(path) = hits.last() {
        match std::fs::read_to_string(path) {
            Ok(content) => return parse_bound_content(&content, path, false),
            Err(e) => {
                return BoundRead::Unreadable(format!("{}: {e}", path.display()));
            }
        }
    }
    BoundRead::None
}

/// Parse one manifest's text into the bound reading. `session` rides the
/// loopcheck scanner because finalize's `harness_session_id` field is private.
fn parse_bound_content(content: &str, path: &Path, live: bool) -> BoundRead {
    let fields = crate::finalize::parse_manifest_fields(content);
    BoundRead::Read(BoundManifestRead {
        node_id: fields.graph_node_id,
        approved: fields.auto_merge_approved,
        source: fields.auto_merge_source,
        session: crate::loopcheck::scan_manifest_field(content, "harness_session_id"),
        manifest_path: path.to_path_buf(),
        live,
    })
}

/// The manifest of the worktree on `branch`, for readers that bind by the
/// PR's head ref. No matching worktree reads as `None` (legacy behavior).
pub(crate) fn branch_bound_manifest(cwd: &Path, branch: &str) -> BoundRead {
    if branch.is_empty() {
        return BoundRead::None;
    }
    let Ok(listed) = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
    else {
        return BoundRead::None;
    };
    if !listed.status.success() {
        return BoundRead::None;
    }
    let matched = parse_worktree_list(&String::from_utf8_lossy(&listed.stdout))
        .into_iter()
        .find(|e| e.branch.as_deref() == Some(branch) && Path::new(&e.path).is_dir());
    match matched {
        Some(e) => read_bound_manifest(Path::new(&e.path)),
        None => BoundRead::None,
    }
}

/// The node-keyed binding for the durable verdict: the worktrees whose bound
/// manifest names this node. `Err` = the binding is ambiguous (two or more
/// live binds) or every candidate is unreadable - either way the verdict
/// fails closed. `Ok(None)` = no binding: legacy behavior.
pub(crate) fn bound_node_posture(
    root: &Path,
    node_id: &str,
) -> Result<Option<BoundManifestRead>, String> {
    let mut live_binds: Vec<BoundManifestRead> = Vec::new();
    let mut archived_binds: Vec<BoundManifestRead> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for wt in worktree_paths(root) {
        match read_bound_manifest(&wt) {
            BoundRead::None => {}
            BoundRead::Unreadable(why) => unreadable.push(why),
            BoundRead::Read(r) => {
                if r.node_id.as_deref() != Some(node_id) {
                    continue;
                }
                if r.live {
                    live_binds.push(r);
                } else {
                    archived_binds.push(r);
                }
            }
        }
    }
    // A valid live manifest outranks archives across trees, not only within
    // one: a re-dispatched node's fresh bind decides over the predecessor's
    // archived form.
    if live_binds.len() > 1 || (live_binds.is_empty() && archived_binds.len() > 1) {
        let binds = if live_binds.is_empty() {
            &archived_binds
        } else {
            &live_binds
        };
        return Err(format!(
            "{} worktrees bind node {node_id}: {}",
            binds.len(),
            binds
                .iter()
                .map(|b| b.manifest_path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(bound) = live_binds.pop() {
        return Ok(Some(bound));
    }
    if let Some(bound) = archived_binds.pop() {
        return Ok(Some(bound));
    }
    if unreadable.is_empty() {
        return Ok(None);
    }
    Err(format!(
        "unreadable bound manifests: {}",
        unreadable.join("; ")
    ))
}

/// What the node's bound manifest does to a merge authority.
#[derive(Debug)]
enum ManifestFold {
    /// No worktree binds the node: legacy receipts-and-config behavior.
    Unbound,
    /// A binding was read; its posture permits (or says nothing). Provenance
    /// rides along; the authority stands.
    Permit(BoundManifestRead),
    /// A binding was read and refuses: `auto_merge_approved: false`.
    Refuse(BoundManifestRead),
    /// The binding is unreadable or ambiguous. Fails closed.
    Unknown(String),
}

/// Read the node's bound-manifest posture once for the fold.
fn bound_fold(root: &Path, node_id: &str) -> ManifestFold {
    match bound_node_posture(root, node_id) {
        Ok(Some(bound)) => {
            if bound.approved == Some(false) {
                ManifestFold::Refuse(bound)
            } else {
                ManifestFold::Permit(bound)
            }
        }
        Ok(None) => ManifestFold::Unbound,
        Err(why) => ManifestFold::Unknown(why),
    }
}

/// The additive provenance a read binding carries onto a verdict receipt or
/// a queue row: which manifest binds, whose session it is, live or archived,
/// and the manifest's own fold of the merge posture.
fn write_provenance(out: &mut Value, bound: &BoundManifestRead) {
    out["manifest_path"] = json!(bound.manifest_path.display().to_string());
    out["bound_session"] = bound
        .session
        .clone()
        .map(|s| json!(s))
        .unwrap_or(Value::Null);
    out["manifest_state"] = json!(if bound.live { "live" } else { "archived" });
    out["auto_merge_source"] = bound
        .source
        .clone()
        .map(|s| json!(s))
        .unwrap_or(Value::Null);
}
/// One `grant-verdict` answer over already-read rows. An `Err` rows read is
/// an unknown verdict naming it - an unread graph never grants. The receipt
/// rides the graph receipt and the live config, and the folded posture of
/// the PR's own bound target manifest.
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
    let mut out = json!({
        "state": verdict.state,
        "reason": verdict.reason,
        "node_id": verdict.node_id,
        "claim_state": verdict.claim_state,
        "grant": verdict.grant,
    });
    // The PR's own bound manifest folds over the receipt+config verdict. It
    // only ever downgrades: `auto_merge_approved: false` refuses, an
    // unreadable or ambiguous binding reads unknown, and provenance lands
    // whenever a binding was read. A projection never widens the merge
    // verb's own gates.
    if let Some(node_id) = verdict.node_id.clone() {
        match bound_fold(&root, &node_id) {
            ManifestFold::Unbound => {}
            ManifestFold::Permit(bound) => {
                write_provenance(&mut out, &bound);
            }
            ManifestFold::Refuse(bound) => {
                write_provenance(&mut out, &bound);
                if verdict.state == GRANTED {
                    let live_word = if bound.live {
                        "live"
                    } else {
                        "newest archived"
                    };
                    out["state"] = json!(REFUSED);
                    out["downgraded_by_manifest"] = json!(true);
                    out["reason"] = json!(format!(
                        "bound target manifest refuses autonomous merge \
(auto_merge_approved: false, auto_merge_source: {src}, harness_session_id: {sess}, \
manifest: {path}, {live_word} manifest)",
                        src = bound.source.as_deref().unwrap_or("unknown"),
                        sess = bound.session.as_deref().unwrap_or("unattributed"),
                        path = bound.manifest_path.display(),
                    ));
                }
            }
            ManifestFold::Unknown(why) => {
                out["manifest_unreadable_or_ambiguous"] = json!(why);
                if verdict.state == GRANTED {
                    out["state"] = json!(UNKNOWN);
                    out["downgraded_by_manifest"] = json!(true);
                    out["reason"] = json!(why);
                }
            }
        }
    }
    out.to_string()
}

/// The queued row's node id, empty when absent.
fn row_node_id(row: &Value) -> &str {
    row.get("node_id").and_then(Value::as_str).unwrap_or("")
}

/// One `grant-queue` answer over already-read rows. An `Err` rows read is an
/// error receipt - the caller refuses its tick's merge work, it never
/// guesses a queue. The receipt carries `elapsed_ms` so the caller can see
/// the read's cost. The bound-manifest fold runs after the pure queue: a
/// granted row whose node's bound manifest refuses, or whose binding cannot
/// be read, drops. A queue only ever narrows the merge verb's gates.
pub fn queue_op(rows: Result<Vec<Value>, String>, rotate: u64, started: Instant) -> Value {
    match rows {
        Err(e) => json!({
            "error": format!("graph unreadable: {e}"),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
        Ok(entries) => {
            let mut out = queue_from_entries(
                &entries,
                &|k| claim_status(k, None).0,
                &|entry: &Value| {
                    entry
                        .get("cwd")
                        .and_then(Value::as_str)
                        .and_then(|cwd| canonical_repo_root(Path::new(cwd)))
                },
                &live_config,
                rotate,
            );
            let root_of = |row: &Value| {
                row.get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_default()
            };
            let mut refused_drops = 0usize;
            let mut unknown_drops = 0usize;
            if let Some(rows) = out["queue"].as_array_mut() {
                rows.retain_mut(|row| match bound_fold(&root_of(row), row_node_id(row)) {
                    ManifestFold::Unbound => true,
                    ManifestFold::Permit(bound) => {
                        write_provenance(row, &bound);
                        true
                    }
                    ManifestFold::Refuse(_) => {
                        refused_drops += 1;
                        false
                    }
                    ManifestFold::Unknown(_) => {
                        unknown_drops += 1;
                        false
                    }
                });
            }
            for (key, by) in [
                ("granted", -(refused_drops as i64 + unknown_drops as i64)),
                ("refused", refused_drops as i64),
                ("unknown", unknown_drops as i64),
            ] {
                if let Some(c) = out["verdicts"].get_mut(key) {
                    *c = json!(c.as_i64().unwrap_or(0) + by);
                }
            }
            out["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
            out
        }
    }
}

/// Dispatch one `grant-` op from an `authorized-merge` payload. Always
/// answers with a JSON receipt; the verb's exit status answers only whether
/// the op RAN.
pub fn run_op(op: &str, payload: &Value) -> String {
    match op {
        "grant-verdict" => {
            // A missing pr narrows to number 0, which no node carries, so the
            // verdict reads the same `absent` the full read answers.
            let pr = payload.get("pr").and_then(Value::as_i64).or(Some(0));
            verdict_op(read_rows(payload, pr), payload)
        }
        "grant-queue" => {
            let started = Instant::now();
            let rotate = payload.get("rotate").and_then(Value::as_u64).unwrap_or(0);
            queue_op(read_rows(payload, None), rotate, started).to_string()
        }
        other => json!({"error": format!("unknown op {other}")}).to_string(),
    }
}

fn read_rows(payload: &Value, pr: Option<i64>) -> Result<Vec<Value>, String> {
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let graph_path = graph_json_path(Path::new(cwd));
    crate::graph_store::read_pr_rows(&graph_path, pr)
        .map(|rows| backlog_api::rows_in(&rows))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
