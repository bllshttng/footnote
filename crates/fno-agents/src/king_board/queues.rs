//! The operator lane parser and the thirteen-queue board build (pure; no I/O).
use super::classify::{claim_is_dead, holder_token, node_driver, node_has_pr};
use super::prs::derived_status;
use super::scope::operator_lane_path;
use super::{
    as_int, s_str, SourceRead, DEAD_CLAIM_STATES, KING_PRIORITIES, LEGACY_DEFER_PREFIX, SRC_CLAIMS,
    SRC_DISTRESS, SRC_DRIVERS, SRC_NEEDS, SRC_PRS, SRC_PR_GATE, SRC_PR_NODES, SRC_QUESTIONS,
    SRC_READY, SRC_UNDISPATCHED, SRC_WORKED, TERMINAL_RUNGS,
};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// Per-project rows rendered for the capture stream; the count stays whole.
pub(crate) const CAPTURE_PROJECT_CAP: usize = 8;

pub(crate) const NODE_ID_BODY: &str = "[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}";

// ---------------------------------------------------------------------------
// Lane: the operator's own ranked file (king/lane.py)
// ---------------------------------------------------------------------------

pub(crate) struct LaneItem {
    pub(crate) text: String,
    pub(crate) node: Option<String>,
    pub(crate) parked: Option<String>,
    pub(crate) done: bool,
    pub(crate) line: usize,
}

pub(crate) fn parse_lane(path: &Path) -> Result<Vec<LaneItem>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read operator lane {}: {e}", path.display())),
    };
    let item_re = regex::Regex::new(r"^- \[( |x|X)\] (.*)$").expect("static regex");
    let body = "[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}";
    let suffix_re = regex::Regex::new(&format!(
        r"->\s*(?:(?P<node>{body})|parked:\s*(?P<reason>\S.*?))\s*$"
    ))
    .expect("static regex");
    let mut items = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let Some(caps) = item_re.captures(raw) else {
            continue;
        };
        let done = &caps[1] != " ";
        let rest = caps[2].to_string();
        let (mut node, mut parked, mut text_out) = (None, None, rest.clone());
        if let Some(sc) = suffix_re.captures(&rest) {
            node = sc.name("node").map(|m| m.as_str().to_string());
            parked = sc.name("reason").map(|m| m.as_str().to_string());
            text_out = rest[..sc.get(0).unwrap().start()].trim_end().to_string();
        }
        items.push(LaneItem {
            text: text_out.trim().to_string(),
            node,
            parked,
            done,
            line: i + 1,
        });
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Distress journal: the `blocked` rows distress.rs already writes
// ---------------------------------------------------------------------------

/// One `<help>` distress row a king board candidate reads: the fields
/// `blocked_child` needs, pulled out of the raw envelope
/// (`{ts, v, type, source, run, node, data: {reason, evidence}}`).
pub(crate) struct BlockedRow {
    pub(crate) ts: String,
    pub(crate) session: String,
    pub(crate) node: Option<String>,
    pub(crate) reason: String,
    pub(crate) evidence: Option<String>,
}

/// Every `type: "blocked"` row in one journal file, oldest-line-first (the
/// file is append-only). A missing file reads as an honest empty list - a
/// king board with nothing blocked yet must not read as unreadable.
pub(crate) fn read_blocked_rows(path: &Path) -> Result<Vec<BlockedRow>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // a corrupt line is skipped, never a whole-file refusal
        };
        if v.get("type").and_then(Value::as_str) != Some("blocked") {
            continue;
        }
        let (Some(ts), Some(session)) = (s_str(&v, "ts"), s_str(&v, "run")) else {
            continue; // the two fields every row this queue needs must be present
        };
        out.push(BlockedRow {
            ts: ts.to_string(),
            session: session.to_string(),
            node: s_str(&v, "node").map(str::to_string),
            reason: v
                .pointer("/data/reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            evidence: v
                .pointer("/data/evidence")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(out)
}

/// Oldest-per-session rows, past their grace window, that closing (node in
/// `TERMINAL_RUNGS`) and claim release (no current claim, or one in
/// `DEAD_CLAIM_STATES`) do NOT already answer. Pure over already-fetched
/// inputs, so a unit test needs no filesystem, subprocess, or clock -
/// exactly the split `build_board` uses for every other queue's inputs.
/// "Released" reads the CURRENT claim only: this scan carries no prior
/// holder to diff against, so gone or dead-stated counts as released and
/// still-live does not. Returns `(row, age_minutes)` - the survivors still
/// need the ONE batched mail-answered check the caller makes.
pub(crate) fn resolve_blocked_child_candidates(
    rows: Vec<BlockedRow>,
    claim_state_by_node: &HashMap<String, String>,
    status_by_node: &HashMap<String, String>,
    grace_minutes: i64,
    now_s: i64,
) -> Vec<(BlockedRow, i64)> {
    // The writer already dedups an identical (run, reason) repeat, but a
    // session can still carry more than one DISTINCT reason - the board
    // shows the oldest, never one row per repeat.
    let mut oldest_by_session: HashMap<String, BlockedRow> = HashMap::new();
    for row in rows {
        match oldest_by_session.get(&row.session) {
            Some(existing) if existing.ts <= row.ts => {}
            _ => {
                oldest_by_session.insert(row.session.clone(), row);
            }
        }
    }
    let mut out = Vec::new();
    for (_session, row) in oldest_by_session {
        let closed = row
            .node
            .as_deref()
            .and_then(|n| status_by_node.get(n))
            .map(|s| TERMINAL_RUNGS.contains(&s.as_str()))
            .unwrap_or(false);
        let claim_released = match row.node.as_deref().and_then(|n| claim_state_by_node.get(n)) {
            None => true,
            Some(state) => DEAD_CLAIM_STATES.contains(&state.as_str()),
        };
        if closed || claim_released {
            continue;
        }
        let row_epoch = crate::tick_ledger::parse_rfc3339_unix(&row.ts)
            .map(|s| s as i64)
            .unwrap_or(now_s);
        let age_minutes = (now_s - row_epoch) / 60;
        if age_minutes < grace_minutes {
            continue;
        }
        out.push((row, age_minutes));
    }
    out
}

/// Does a bus address reach this blocked row? A target row is keyed by its
/// run id, which the bus never addresses; the node's live claim names the
/// harness session the bus does address. The harness-shape guard keeps a run
/// id from matching by its first 8 characters, which are a date - valid hex,
/// so a bare handle could collide with it.
pub(crate) fn addresses_row(addr: &str, row_session: &str, holder: Option<&str>) -> bool {
    addr == row_session
        || (crate::identity::harness_of_session_id(row_session).is_some()
            && crate::identity::session_handle_tier(addr, row_session).is_some())
        || holder.is_some_and(|h| crate::identity::session_handle_tier(addr, h).is_some())
}

/// Mail answered a row when a bus line reaches it (row key or live claim
/// holder) after the row's ts and does not also come from the row itself -
/// self-sends (a worker's own review triggers) are not answers, and a line
/// with no `from` is not a self-send. Candidates are `(row session, holder
/// session, row ts)`; the result stays keyed by row session.
pub(crate) fn mail_answered_since(
    live_log: &Path,
    candidates: &[(String, Option<String>, String)],
) -> HashMap<String, bool> {
    let mut answered: HashMap<String, bool> = candidates
        .iter()
        .map(|(s, _, _)| (s.clone(), false))
        .collect();
    for segment in bus_segments_oldest_first(live_log) {
        let Ok(text) = std::fs::read_to_string(&segment) else {
            continue;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let (Some(to), Some(ts)) = (s_str(&v, "to"), s_str(&v, "ts")) else {
                continue;
            };
            let from = s_str(&v, "from");
            for (session, holder, cutoff) in candidates {
                let holder = holder.as_deref();
                if ts > cutoff.as_str()
                    && addresses_row(to, session, holder)
                    && !from.is_some_and(|f| addresses_row(f, session, holder))
                {
                    answered.insert(session.clone(), true);
                }
            }
        }
    }
    answered
}

/// Retained bus log segments oldest -> newest: rotated `.N` (high N first),
/// then the live file - mirrors `bus/log.py::_segment_paths_oldest_first`.
fn bus_segments_oldest_first(live: &Path) -> Vec<std::path::PathBuf> {
    let mut rotated: Vec<(u32, std::path::PathBuf)> = Vec::new();
    if let (Some(parent), Some(name)) = (live.parent(), live.file_name()) {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let prefix = format!("{}.", name.to_string_lossy());
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if let Some(n) = fname
                    .strip_prefix(&prefix)
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    rotated.push((n, entry.path()));
                }
            }
        }
    }
    rotated.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: Vec<std::path::PathBuf> = rotated.into_iter().map(|(_, p)| p).collect();
    if live.exists() {
        out.push(live.to_path_buf());
    }
    out
}

/// The final blocked_child rows: every candidate the mail-answered check
/// did NOT clear, rendered as the row shape the queue emits. Pure.
pub(crate) fn filter_unanswered_by_mail(
    candidates: Vec<(BlockedRow, i64)>,
    mail_answered: &HashMap<String, bool>,
    watchdog_verdicts: &HashMap<String, String>,
) -> Vec<Value> {
    candidates
        .into_iter()
        .filter(|(row, _)| !mail_answered.get(&row.session).copied().unwrap_or(false))
        .map(|(row, age_minutes)| {
            json!({
                "id": row.node,
                "session": row.session,
                "reason": row.reason,
                "evidence": row.evidence,
                "age_minutes": age_minutes,
                "watchdog_verdict": watchdog_verdicts.get(&row.session),
            })
        })
        .collect()
}

/// The watchdog verdict for a blocked row: the payload is keyed by harness
/// session, so look up the row key first and the claim holder session second.
pub(crate) fn verdict_for(
    payload: Option<&Value>,
    row_session: &str,
    holder: Option<&str>,
) -> Option<String> {
    let p = payload?;
    p.get(row_session)
        .or_else(|| holder.and_then(|h| p.get(h)))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// One row per node whose plan artifacts hold an open prove-it FAIL
/// the verdict that used to route nowhere.
fn verdict_rows_from(outstanding: &Value) -> Vec<Value> {
    outstanding
        .get("verdicts")
        .and_then(|v| v.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|r| {
                    json!({
                        "node": r.get("node"),
                        "verdict": r.get("verdict"),
                        "report": r.get("report"),
                        "claim": r.get("claim"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Board construction: the thirteen queues
// ---------------------------------------------------------------------------

pub(crate) struct Queue {
    pub(crate) name: &'static str,
    pub(crate) source: String,
    pub(crate) status: &'static str,
    pub(crate) error: String,
    pub(crate) count: i64,
    pub(crate) rows: Vec<Value>,
    pub(crate) actionable: bool,
    pub(crate) note: String,
    pub(crate) verb: &'static str,
}

/// The not-read statuses, decided once. A budget kill is `over_budget`;
/// every other failed read is `unreadable`. Both mean the queue answered
/// nothing, so every not-read consumer (null count, tally, exit code,
/// termination rendering) reads this predicate instead of re-deciding.
pub(crate) fn not_read_status(status: &str) -> bool {
    status == "unreadable" || status == "over_budget"
}

pub(crate) fn queue(
    name: &'static str,
    source: String,
    read: &SourceRead,
    rows: Vec<Value>,
    actionable: bool,
    note: String,
    verb: &'static str,
    count: Option<i64>,
) -> Queue {
    if !read.is_ok() {
        return Queue {
            name,
            source,
            status: if read.over_budget {
                "over_budget"
            } else {
                "unreadable"
            },
            error: read.error.clone().unwrap_or_default(),
            count: -1,
            rows: Vec::new(),
            actionable,
            note,
            verb,
        };
    }
    Queue {
        name,
        source,
        status: "ok",
        error: String::new(),
        count: count.unwrap_or(rows.len() as i64),
        rows,
        actionable,
        note,
        verb,
    }
}

pub(crate) fn queue_json(q: &Queue) -> Value {
    json!({
        "name": q.name,
        "source": q.source,
        "status": q.status,
        "error": q.error,
        "count": if not_read_status(q.status) { Value::Null } else { json!(q.count) },
        "rows": q.rows,
        "actionable": q.actionable,
        "note": q.note,
        "verb": q.verb,
    })
}

/// All the board's fetched sources, ready for the pure build.
pub(crate) struct BoardInputs {
    pub(crate) ready: SourceRead,
    pub(crate) claims: SourceRead,
    pub(crate) worked: SourceRead,
    pub(crate) claimed_nodes: SourceRead,
    /// The driver feed: registry rows that target a node, the
    /// roster-side answer to "who drives" that the claim snapshot cannot
    /// give. Every node_driver-consuming queue reads unreadable when this
    /// fails, the same fold holder_activity_error gets.
    pub(crate) drivers: SourceRead,
    pub(crate) holder_activity: HashMap<String, crate::truth_probe::TruthProbe>,
    /// The truth batch's failure receipt: `Some` when the batch timed out or
    /// its reader panicked. The claim-dependent queues read unreadable
    /// rather than rendering an absent measurement as a verdict.
    pub(crate) holder_activity_error: Option<String>,
    pub(crate) prs: SourceRead,
    pub(crate) pr_nodes: SourceRead,
    /// The merge gate's verdict per candidate: `fno do pr status`'s
    /// `ready` + `ready_blockers`, one row per PR the listing called green
    ///. A candidate absent from here has no gate answer; build
    /// renders it not-actionable rather than trusting the listing alone.
    pub(crate) pr_gates: SourceRead,
    pub(crate) outstanding: SourceRead,
    pub(crate) needs: SourceRead,
    pub(crate) lane: SourceRead,
    pub(crate) undispatched: SourceRead,
    /// Pre-computed blocked_child candidates: one row per session with an
    /// unanswered `blocked` distress row past `blocked_child_grace_minutes`,
    /// already carrying `id`/`session`/`reason`/`evidence`/`age_minutes`.
    /// The answered/unanswered decision (mail, claim release, node closing)
    /// happens during collection, where the claim/graph/mail sources it
    /// needs already live; this queue only scope-filters and renders, the
    /// same split `undispatched` uses for its Python-computed selection.
    pub(crate) blocked_child: SourceRead,
    /// The graph entries (None = unreadable); one read shared with scope
    /// compile, undispatched classify, and claimed-node lookups.
    pub(crate) entries: Option<Vec<Value>>,
    /// Held nodes (x-55ae): node -> the open question id that holds it. The
    /// ready feed has already partitioned them out; the undispatched queue
    /// reads the same map so a held node never reads as stuck work.
    pub(crate) held: std::collections::BTreeMap<String, String>,
    pub(crate) warnings: Vec<String>,
    pub(crate) autonomous_merge: bool,
    pub(crate) scope_ids: Option<HashSet<String>>,
    pub(crate) crown_scope: Option<String>,
}

/// True when any non-done descendant under `parent_id` is being driven: a
/// live claim (even one whose holder probe did not answer - the lock is
/// real), a crown, or a worked-feed listing. Container children recurse: a
/// sub-epic carries no claim of its own, but its leaf's driver holds the
/// whole chain. A claim-free child under an unreadable worked feed names no
/// driver, so measurement blindness can never read as held. `seen` breaks
/// parent cycles; ids are matched lowercased.
fn subtree_held(
    parent_id: &str,
    children_by_parent: &HashMap<String, Vec<&Value>>,
    claim_by_node: &HashMap<String, Value>,
    activity: &HashMap<String, crate::truth_probe::TruthProbe>,
    crown_ids: Option<&HashSet<String>>,
    worked: Option<&SourceRead>,
    drivers: Option<&SourceRead>,
    seen: &mut HashSet<String>,
) -> bool {
    if parent_id.is_empty() || !seen.insert(parent_id.to_string()) {
        return false;
    }
    children_by_parent.get(parent_id).is_some_and(|children| {
        children.iter().any(|child| {
            let done = derived_status(child) == "done"
                || s_str(child, "status")
                    .map(|s| TERMINAL_RUNGS.contains(&s))
                    .unwrap_or(false)
                || child.get("superseded_by").is_some_and(|v| !v.is_null());
            if done {
                return false;
            }
            let (state, claim) =
                node_driver(child, claim_by_node, activity, crown_ids, worked, drivers);
            let live_claim = claim.is_some_and(|c| !claim_is_dead(c, activity));
            if live_claim || state == "crowned" {
                return true;
            }
            let worked_driven = state == "active"
                && claim.is_none()
                && s_str(child, "contained_in").is_none_or(|c| c.is_empty());
            if worked_driven {
                return true;
            }
            match s_str(child, "id") {
                Some(child_id) => subtree_held(
                    &child_id.to_ascii_lowercase(),
                    children_by_parent,
                    claim_by_node,
                    activity,
                    crown_ids,
                    worked,
                    drivers,
                    seen,
                ),
                None => false,
            }
        })
    })
}

/// The aggregate the termination readers key on: a FLOOR, the rows the board
/// can actually name. Blind actionable queues contribute nothing and are
/// named in a warning instead (: +1 per blind queue read as one row
/// and a king went hunting rows that never existed). A blind REPORT-ONLY
/// queue stays loud through unreadable and uncounted.
pub(crate) fn actionable_tally(queues: &[Queue], warnings: &mut Vec<String>) -> i64 {
    let mut readable: i64 = 0;
    let mut blind: Vec<&'static str> = Vec::new();
    for q in queues {
        if not_read_status(q.status) {
            if q.actionable {
                blind.push(q.name);
            }
        } else if q.actionable {
            readable += q.count;
        }
    }
    if !blind.is_empty() {
        blind.sort();
        warnings.push(format!(
            "actionable is a floor: {} actionable queue(s) unreadable and uncounted ({})",
            blind.len(),
            blind.join(", ")
        ));
    }
    readable
}

/// Build the board payload. Pure; does no I/O. Queue names, order, and row
/// shapes match board.py's `build_board` exactly.
pub(crate) fn build_board(inputs: &BoardInputs) -> Value {
    let mut warnings = inputs.warnings.clone();
    let mut out_of_scope: Vec<Value> = Vec::new();
    // On a scoped board a row whose node id cannot be resolved is
    // unknown, not mine. Work rows fail CLOSED - they land in no queue and
    // are named in one warning line below - because "in scope" would hand
    // every crown every unattributable PR, while out_of_scope would mislabel
    // the row as another crown's work. A REPORT-ONLY queue (unreachable_worker)
    // bypasses instead: its signals carry no node by design (needs.rs mints
    // node: None for mail_escalation, carveout_stale, stale_claims,
    // worker_refused) and the evidence must reach every board. The unscoped
    // arm returns true before any of this: the operator board keeps showing
    // every row.
    let unattributed: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let scope_ids = inputs.scope_ids.as_ref();

    let in_scope = |queue: &str,
                    report_only: bool,
                    node_id: &Value,
                    row: &Value,
                    out: &mut Vec<Value>|
     -> bool {
        let Some(ids) = scope_ids else {
            return true;
        };
        let Some(id) = node_id.as_str() else {
            if report_only {
                return true;
            }
            unattributed.borrow_mut().push(queue.to_string());
            return false;
        };
        if ids.contains(id) {
            return true;
        }
        let mut extra = Map::new();
        extra.insert("queue".to_string(), json!(queue));
        extra.insert("id".to_string(), json!(id));
        if let Some(title) = row.get("title").filter(|t| !t.is_null()) {
            extra.insert("title".to_string(), title.clone());
        }
        out.push(Value::Object(extra));
        false
    };

    let claim_rows = inputs.claims.rows();
    let mut claim_by_node: HashMap<String, Value> = HashMap::new();
    for row in &claim_rows {
        if let Some(key) = s_str(row, "key") {
            if let Some(node_id) = key.strip_prefix("node:") {
                claim_by_node.insert(node_id.to_string(), row.clone());
            }
        }
    }

    // a holder the probe batch never answered for is a hole in the
    // board's evidence, not a worker verdict. Name every hole in one warning
    // line so a partially-answered batch is visible in the payload, not only
    // through the rows its absence silently removed. The expected set is the
    // CLAIM-derived subset of the probe feed (king-priority claimed nodes +
    // dead-state claims; roster driver tokens are also fed to the probe but
    // never warned about): a live claim on a lower-priority node is never fed
    // to the probe, so counting it here would warn forever about a holder
    // nobody promised to measure.
    let mut unmeasured_holders: Vec<String> = Vec::new();
    if inputs.holder_activity_error.is_none() {
        let probed_ids: HashSet<String> = inputs
            .claimed_nodes
            .rows()
            .iter()
            .filter(|n| KING_PRIORITIES.contains(&s_str(n, "priority").unwrap_or("")))
            .filter_map(|n| s_str(n, "id").map(str::to_string))
            .collect();
        let mut expected: HashSet<String> = HashSet::new();
        for row in &claim_rows {
            let token = holder_token(row);
            if token.is_empty() {
                continue;
            }
            let dead_state = DEAD_CLAIM_STATES.contains(&s_str(row, "state").unwrap_or(""));
            let node_id = s_str(row, "key")
                .and_then(|k| k.strip_prefix("node:"))
                .unwrap_or("");
            if dead_state || probed_ids.contains(node_id) {
                expected.insert(token);
            }
        }
        for token in &expected {
            if !inputs.holder_activity.contains_key(token) {
                unmeasured_holders.push(token.clone());
            }
        }
        unmeasured_holders.sort();
    }
    if !unmeasured_holders.is_empty() {
        warnings.push(format!(
            "holder_activity: {} holder(s) unmeasured: {}",
            unmeasured_holders.len(),
            unmeasured_holders.join(", ")
        ));
    }

    // Undispatched: planned work with no claim, king priorities only.
    let undispatched_rows = if inputs.undispatched.is_ok() {
        inputs
            .undispatched
            .rows()
            .into_iter()
            .filter(|node| KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")))
            .filter(|node| {
                // A held node has a named question, not a missing dispatch
                // (x-55ae); it renders under held, never here.
                !node
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| inputs.held.contains_key(id))
            })
            .filter(|node| {
                in_scope(
                    "undispatched",
                    false,
                    node.get("id").unwrap_or(&Value::Null),
                    node,
                    &mut out_of_scope,
                )
            })
            .map(|node| {
                json!({
                    "id": node.get("id"),
                    "priority": node.get("priority"),
                    "title": node.get("title"),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Unplanned: cold-dispatchable ideas off the ready list.
    let unplanned_rows: Vec<Value> = inputs
        .ready
        .rows()
        .into_iter()
        .filter(|node| KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")))
        .filter(|node| {
            node.get("plan_path")
                .map(|p| p.is_null() || p.as_str().map(|s| s.is_empty()).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|node| {
            // the ready feed drops claimed nodes it cannot see
            // (non-stale claims are excluded there, worked ids too), so the
            // driver join is the only read left. A node under ANY driver -
            // active, stalled, unmeasured, or a dead claim that belongs to
            // stale_claim - is not unplanned.
            let (state, claim) = node_driver(
                node,
                &claim_by_node,
                &inputs.holder_activity,
                inputs.scope_ids.as_ref(),
                Some(&inputs.worked),
                Some(&inputs.drivers),
            );
            state == "none" && claim.is_none()
        })
        .filter(|node| {
            in_scope(
                "unplanned",
                false,
                node.get("id").unwrap_or(&Value::Null),
                node,
                &mut out_of_scope,
            )
        })
        .map(|node| {
            json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
            })
        })
        .collect();

    // Stalled holder: starts from the CLAIM, never the ready list (a live
    // holder is exactly what `ready` has already removed).
    let mut stalled_rows: Vec<Value> = Vec::new();
    for node in &inputs.claimed_nodes.rows() {
        if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
            continue;
        }
        if s_str(node, "status")
            .map(|s| TERMINAL_RUNGS.contains(&s))
            .unwrap_or(false)
        {
            continue;
        }
        let (state, claim) = node_driver(
            node,
            &claim_by_node,
            &inputs.holder_activity,
            inputs.scope_ids.as_ref(),
            Some(&inputs.worked),
            Some(&inputs.drivers),
        );
        if state != "stalled" {
            continue;
        }
        let claim = claim.expect("stalled always carries its claim");
        if !in_scope(
            "stalled_holder",
            false,
            node.get("id").unwrap_or(&Value::Null),
            &node,
            &mut out_of_scope,
        ) {
            continue;
        }
        stalled_rows.push(json!({
            "id": node.get("id"),
            "priority": node.get("priority"),
            "title": node.get("title"),
            "holder": claim.get("holder"),
            "worker": holder_token(claim),
            "claim_state": claim.get("state"),
        }));
    }

    // Stale claims: locks nobody will reap. Asked of the holder, not the
    // clock alone: an expired lease under a writing worker is a live lock.
    let stale_claim_rows: Vec<Value> = claim_rows
        .iter()
        .filter(|row| claim_is_dead(row, &inputs.holder_activity))
        .filter(|row| {
            // A key that is not `node:`-prefixed is unattributable, not
            // another crown's: feed Null so it fails closed with the other
            // unknown-attribution rows instead of matching `""` out of scope.
            let node_id = s_str(row, "key")
                .and_then(|k| k.strip_prefix("node:"))
                .map(|id| json!(id))
                .unwrap_or(Value::Null);
            in_scope("stale_claim", false, &node_id, row, &mut out_of_scope)
        })
        .map(|row| {
            json!({
                "key": row.get("key"),
                "holder": row.get("holder"),
                "state": row.get("state"),
            })
        })
        .collect();

    // Unheld progress: the status stamp is never revoked when a worker dies,
    // so an in_progress node with no live claim is invisible to every other
    // queue at once - undispatched reads ready, stalled_holder starts from a
    // live claim, undriven_pr requires a PR. Built only on a readable claims
    // list: with no claims read every node would look unheld at once.
    let mut unheld_rows: Vec<Value> = Vec::new();
    if inputs.claims.is_ok() {
        // A container is judged by its children, never by the leaf test:
        // an epic goes in_progress because its children are worked, and
        // the epic itself never takes a claim or opens a PR, so the leaf
        // test flagged every healthy epic forever. Held when any non-done
        // descendant is driven (subtree_held); still reported when none
        // is - an epic whose children all died is exactly what a king
        // must see.
        let mut children_by_parent: HashMap<String, Vec<&Value>> = HashMap::new();
        for entry in inputs.entries.as_deref().unwrap_or(&[]) {
            if let Some(parent) = s_str(entry, "parent") {
                children_by_parent
                    .entry(parent.to_ascii_lowercase())
                    .or_default()
                    .push(entry);
            }
        }
        for node in inputs.entries.as_deref().unwrap_or(&[]) {
            if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
                continue;
            }
            if s_str(node, "status") != Some("in_progress") {
                continue;
            }
            if node.get("superseded_by").is_some_and(|v| !v.is_null()) {
                continue;
            }
            if node
                .get("completed_at")
                .and_then(Value::as_str)
                .is_some_and(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
            {
                continue;
            }
            if node_has_pr(node) {
                continue; // undriven_pr owns the PR-bound shape.
            }
            let (state, claim) = node_driver(
                node,
                &claim_by_node,
                &inputs.holder_activity,
                inputs.scope_ids.as_ref(),
                Some(&inputs.worked),
                Some(&inputs.drivers),
            );
            if state != "none" {
                continue;
            }
            let mut seen: HashSet<String> = HashSet::new();
            if subtree_held(
                s_str(node, "id").unwrap_or(""),
                &children_by_parent,
                &claim_by_node,
                &inputs.holder_activity,
                inputs.scope_ids.as_ref(),
                Some(&inputs.worked),
                Some(&inputs.drivers),
                &mut seen,
            ) {
                continue;
            }
            if !in_scope(
                "unheld_progress",
                false,
                node.get("id").unwrap_or(&Value::Null),
                node,
                &mut out_of_scope,
            ) {
                continue;
            }
            let mut row = json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
            });
            if let Some(claim) = claim {
                row.as_object_mut().unwrap().insert(
                    "claim_state".to_string(),
                    claim.get("state").cloned().unwrap_or(Value::Null),
                );
            }
            unheld_rows.push(row);
        }
    }

    // Blocked child: every row `blocked_child` collection already computed
    // as unanswered-past-grace (mail, claim release, node closing all
    // checked at collection time, where those sources live). This build
    // only scope-filters - the same split `undispatched` uses for a
    // Python-computed selection.
    let blocked_child_rows: Vec<Value> = inputs
        .blocked_child
        .rows()
        .into_iter()
        .filter(|row| {
            in_scope(
                "blocked_child",
                false,
                row.get("id").unwrap_or(&Value::Null),
                row,
                &mut out_of_scope,
            )
        })
        .collect();

    // Operator lane.
    let lane_ok = inputs.lane.is_ok();
    let lane_items: Vec<LaneItem> = if lane_ok {
        inputs
            .lane
            .rows()
            .iter()
            .map(|r| LaneItem {
                text: s_str(r, "text").unwrap_or("").to_string(),
                node: r.get("node").and_then(Value::as_str).map(str::to_string),
                parked: r.get("parked").and_then(Value::as_str).map(str::to_string),
                done: r.get("done").and_then(Value::as_bool).unwrap_or(false),
                line: r.get("line").and_then(Value::as_u64).unwrap_or(0) as usize,
            })
            .collect()
    } else {
        Vec::new()
    };
    let lane_open: Vec<&LaneItem> = lane_items
        .iter()
        .filter(|i| !i.done && i.node.is_none() && i.parked.is_none())
        .collect();
    let parked_count = lane_items.iter().filter(|i| i.parked.is_some()).count();
    let mut lane_note = "the operator's own ranking. File each with `fno backlog idea \"<text>\"` and stamp `-> <id>` onto its line, or park it with `-> parked: <reason>`.".to_string();
    if parked_count > 0 {
        lane_note.push_str(&format!(" {parked_count} parked, reasons are in the file."));
    }
    let scoped = scope_ids.is_some();
    if scoped {
        lane_note.push_str(" report-only under a crown: lane lines are the operator's global priorities and carry no node id, so a scoped king cannot attribute them to its subtree");
    }
    let lane_rows: Vec<Value> = if lane_ok {
        lane_open
            .iter()
            .map(|i| json!({"text": i.text, "line": i.line}))
            .collect()
    } else {
        Vec::new()
    };

    // mergeable_pr is scoped by the node that binds each PR (pr_number plus
    // additional_prs), the same join undriven_pr makes. A PR no node claims
    // stays visible on the unscoped board; on a scoped board it fails closed
    // and is named in `warnings` rather than read as every crown's work.
    let mut node_by_pr: HashMap<i64, String> = HashMap::new();
    for node in &inputs.pr_nodes.rows() {
        let Some(node_id) = s_str(node, "id").map(str::to_string) else {
            continue;
        };
        if let Some(n) = node.get("pr_number").and_then(Value::as_i64) {
            node_by_pr.insert(n, node_id.clone());
        }
        if let Some(extras) = node.get("additional_prs").and_then(Value::as_array) {
            for extra in extras {
                if let Some(n) = extra.get("number").and_then(Value::as_i64) {
                    node_by_pr.insert(n, node_id.clone());
                }
            }
        }
    }
    // The gate verdict per candidate: the listing proves open+green, the
    // gate proves fusable. A candidate absent from the gate read
    // (its call failed, or the slice ran out) carries `ready: null` and is
    // NOT actionable: a reader that cannot say what it did not read must not
    // offer a merge. Rows stay visible with their blockers rendered beside
    // them, so the row teaches instead of nags.
    let gate_rows = inputs.pr_gates.rows();
    let gate_by_pr: HashMap<i64, &Value> = gate_rows
        .iter()
        .filter_map(|g| {
            let n = g.get("number").and_then(Value::as_i64)?;
            Some((n, g))
        })
        .collect();
    let pr_rows: Vec<Value> = inputs
        .prs
        .rows()
        .iter()
        .filter(|r| {
            let node_id = node_by_pr
                .get(&r.get("number").and_then(Value::as_i64).unwrap_or(-1))
                .map(|id| json!(id))
                .unwrap_or(Value::Null);
            in_scope("mergeable_pr", false, &node_id, r, &mut out_of_scope)
        })
        .map(|r| {
            let n = r.get("number").and_then(Value::as_i64).unwrap_or(-1);
            let (ready, blockers) = match gate_by_pr.get(&n) {
                Some(g) => (
                    g.get("ready").cloned().unwrap_or(Value::Null),
                    g.get("ready_blockers").cloned().unwrap_or(json!([])),
                ),
                None => (Value::Null, json!([])),
            };
            let row_ready = ready == Value::Bool(true);
            json!({
                "number": r.get("number"),
                "title": r.get("title"),
                "ready": ready,
                "ready_blockers": blockers,
                // Row-level veto the termination reader honors: a not-ready
                // row stays visible but names no next action.
                "actionable": row_ready,
            })
        })
        .collect();
    let mergeable_count = pr_rows
        .iter()
        .filter(|r| r.get("actionable").and_then(Value::as_bool) == Some(true))
        .count() as i64;

    // Undriven PR: the complement of stalled_holder, the second half of ONE
    // predicate. Fail CLOSED on an unreadable claim list: every node would
    // read "none" and the king would dispatch over every live worker at once.
    let mergeable_numbers: HashSet<i64> = if inputs.autonomous_merge {
        pr_rows
            .iter()
            .filter(|r| r.get("actionable").and_then(Value::as_bool) == Some(true))
            .filter_map(|r| r.get("number").and_then(Value::as_i64))
            .collect()
    } else {
        HashSet::new()
    };
    let mut undriven_rows: Vec<Value> = Vec::new();
    // task 1.4b: the pr_nodes rows carry no `contained_in` (the field
    // lives on the graph entry), so node_driver's contained arm cannot fire
    // here on its own. Resolve it from the entries the board already holds.
    let contained_ids: HashSet<String> = inputs
        .entries
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|e| s_str(e, "contained_in").is_some_and(|c| !c.is_empty()))
        .filter_map(|e| s_str(e, "id").map(str::to_string))
        .collect();
    // A ruling hold (crown's dispatch_hold) parks the node deliberately;
    // it is not driverless, so undriven_pr must not name it.
    let by_id: BTreeMap<String, Value> = inputs
        .entries
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| s_str(e, "id").map(|id| (id.to_string(), e.clone())))
        .collect();
    if inputs.pr_nodes.is_ok() && inputs.claims.is_ok() {
        for node in &inputs.pr_nodes.rows() {
            // No priority filter: a PR is finished work at any band, so a p2
            // node's open PR is as driverless as a p1's.
            if s_str(node, "id").is_some_and(|id| contained_ids.contains(id)) {
                continue;
            }
            let terminal = s_str(node, "status")
                .map(|s| TERMINAL_RUNGS.contains(&s))
                .unwrap_or(false)
                || node.get("superseded_by").is_some_and(|v| !v.is_null())
                || node
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .map(|c| !c.is_empty() && !c.starts_with(LEGACY_DEFER_PREFIX))
                    .unwrap_or(false);
            if terminal {
                continue;
            }
            let status = derived_status(node);
            if status == "deferred" || status == "blocked" {
                continue;
            }
            // pr_nodes rows carry no `plan_path` (that lives on the graph
            // entry, per the contained_ids comment above); look the node up
            // in `by_id` so the hold reader sees the entry that actually
            // carries the plan.
            let hold_entry = s_str(node, "id")
                .and_then(|id| by_id.get(id))
                .unwrap_or(node);
            if crate::backlog_ready::dispatch_hold_verdict(hold_entry, &by_id)
                .is_some_and(|v| v.held)
            {
                continue;
            }
            let (state, _claim) = node_driver(
                node,
                &claim_by_node,
                &inputs.holder_activity,
                inputs.scope_ids.as_ref(),
                Some(&inputs.worked),
                Some(&inputs.drivers),
            );
            if state != "none" {
                continue;
            }
            let pr_number = node.get("pr_number").and_then(Value::as_i64);
            if let Some(n) = pr_number {
                if mergeable_numbers.contains(&n) {
                    continue;
                }
            }
            if !in_scope(
                "undriven_pr",
                false,
                node.get("id").unwrap_or(&Value::Null),
                &node,
                &mut out_of_scope,
            ) {
                continue;
            }
            undriven_rows.push(json!({
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
                "status": status,
                "pr_number": node.get("pr_number"),
                "pr_url": node.get("pr_url"),
            }));
        }
    }

    // One outstanding read, three streams.
    let outstanding = inputs.outstanding.dict();
    let question_rows: Vec<Value> = outstanding
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .map(|r| {
                    json!({
                        "id": r.get("id"),
                        "question": r.get("question"),
                        "ts": r.get("ts"),
                        "session_id": r.get("session_id")
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let carveout_stream = outstanding
        .get("carveouts")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let mut carveout_by_kind: Vec<(String, i64)> = carveout_stream
        .get("by_kind")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), as_int(v))).collect())
        .unwrap_or_default();
    carveout_by_kind.sort();
    let carveout_rows: Vec<Value> = carveout_by_kind
        .into_iter()
        .map(|(kind, n)| json!({"kind": kind, "n": n}))
        .collect();
    let carveout_root = outstanding
        .get("roots")
        .and_then(|r| r.get("carveouts"))
        .and_then(|c| c.get("root"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let capture_stream = outstanding
        .get("captures")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let capture_by_project: Vec<(String, i64)> = capture_stream
        .get("by_project")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), as_int(v))).collect())
        .unwrap_or_default();
    let total_projects = capture_by_project.len();
    let mut capture_sorted = capture_by_project;
    capture_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut capture_rows: Vec<Value> = capture_sorted
        .iter()
        .take(CAPTURE_PROJECT_CAP)
        .map(|(project, n)| json!({"project": project, "n": n}))
        .collect();
    let elided = total_projects.saturating_sub(capture_rows.len());
    if elided > 0 {
        capture_rows.push(json!({"elided_projects": elided}));
    }

    // One row per node whose plan artifacts hold an open prove-it FAIL
    // the verdict that used to route nowhere. An ok read with no
    // verdicts key at all is a STALE Python leg, not a clean one -- the key is
    // unconditionally emitted since the leg shipped, so its absence must not
    // read as "zero FAILs".
    let verdict_rows = verdict_rows_from(&Value::Object(outstanding.clone()));
    if inputs.outstanding.is_ok() && outstanding.get("verdicts").is_none() {
        warnings.push(
            "the outstanding read carries no verdicts leg; the installed fno Python is \
             older than this binary (fno doctor update)"
                .to_string(),
        );
    }

    // `fno agents needs` emits operator questions in the same list; the queue
    // above already carries them, so this one drops the kind.
    let needs_rows: Vec<Value> = inputs
        .needs
        .rows()
        .iter()
        .filter(|row| s_str(row, "kind") != Some("operator_question"))
        .filter(|row| {
            in_scope(
                "unreachable_worker",
                true,
                row.get("node").unwrap_or(&Value::Null),
                row,
                &mut out_of_scope,
            )
        })
        .map(|row| {
            json!({"kind": row.get("kind"), "name": row.get("name"), "node": row.get("node")})
        })
        .collect();

    let lane_source = format!("cat {}", operator_lane_path(Path::new(".")).display());

    let mut queues = vec![
        queue(
            "operator_lane",
            lane_source,
            &inputs.lane,
            lane_rows,
            !scoped,
            lane_note,
            "",
            None,
        ),
        queue(
            "undispatched",
            format!("{SRC_UNDISPATCHED} + {SRC_CLAIMS}"),
            &if inputs.undispatched.is_ok() && inputs.claims.is_ok() {
                SourceRead::ok(Value::Null)
            } else {
                let combined = inputs
                    .undispatched
                    .error
                    .clone()
                    .or_else(|| inputs.claims.error.clone())
                    .unwrap_or_default();
                // The composed read keeps the louder verdict: a budget kill
                // must not degrade into "unreadable" because a second source
                // was wrapped around it.
                if inputs.undispatched.over_budget || inputs.claims.over_budget {
                    SourceRead::over_budget(combined)
                } else {
                    SourceRead::err(combined)
                }
            },
            undispatched_rows,
            true,
            "one worker per node; these already carry a plan".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "unplanned",
            format!("{SRC_READY} + {SRC_CLAIMS} + {SRC_WORKED} + {SRC_DRIVERS} + holder_activity"),
            &if inputs.ready.is_ok()
                && inputs.claims.is_ok()
                && inputs.worked.is_ok()
                && inputs.drivers.is_ok()
                && inputs.holder_activity_error.is_none()
            {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .ready
                        .error
                        .clone()
                        .or_else(|| inputs.claims.error.clone())
                        .or_else(|| inputs.worked.error.clone())
                        .or_else(|| inputs.drivers.error.clone())
                        .or_else(|| inputs.holder_activity_error.clone())
                        .unwrap_or_default(),
                )
            },
            unplanned_rows,
            true,
            "batch: up to 3 blueprints per session; merge same-shape nodes into one waved plan".to_string(),
            "/fno:blueprint",
            None,
        ),
        queue(
            "stalled_holder",
            format!("{SRC_CLAIMS} + {SRC_DRIVERS} + fno backlog get <id> + fno agents peek <worker>"),
            &if inputs.claims.is_ok()
                && inputs.claimed_nodes.is_ok()
                && inputs.drivers.is_ok()
                && inputs.holder_activity_error.is_none()
            {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .claims
                        .error
                        .clone()
                        .or_else(|| inputs.claimed_nodes.error.clone())
                        .or_else(|| inputs.drivers.error.clone())
                        .or_else(|| inputs.holder_activity_error.clone())
                        .unwrap_or_default(),
                )
            },
            stalled_rows,
            true,
            String::new(),
            "",
            None,
        ),
        queue(
            "unheld_progress",
            format!("graph entries + {SRC_CLAIMS} + {SRC_DRIVERS} + holder_activity"),
            &if inputs.entries.is_some()
                && inputs.claims.is_ok()
                && inputs.drivers.is_ok()
                && inputs.holder_activity_error.is_none()
            {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(if inputs.entries.is_none() {
                    "graph unreadable".to_string()
                } else {
                    inputs
                        .claims
                        .error
                        .clone()
                        .or_else(|| inputs.drivers.error.clone())
                        .or_else(|| inputs.holder_activity_error.clone())
                        .unwrap_or_default()
                })
            },
            unheld_rows,
            true,
            "the status stamp is never revoked when a worker dies; claim-free in_progress rows are dead handoffs - redispatch the node or close it by hand".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "blocked_child",
            SRC_DISTRESS.to_string(),
            &inputs.blocked_child,
            blocked_child_rows,
            true,
            "a child under this crown emitted <help> and nobody answered it inside the grace window - check on it, or mail it to unblock".to_string(),
            "",
            None,
        ),
        queue(
            "undriven_pr",
            format!("{SRC_PR_NODES} + {SRC_CLAIMS} + {SRC_DRIVERS} + holder_activity"),
            &if inputs.pr_nodes.is_ok()
                && inputs.claims.is_ok()
                && inputs.drivers.is_ok()
                && inputs.holder_activity_error.is_none()
            {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .pr_nodes
                        .error
                        .clone()
                        .or_else(|| inputs.claims.error.clone())
                        .or_else(|| inputs.drivers.error.clone())
                        .or_else(|| inputs.holder_activity_error.clone())
                        .unwrap_or_default(),
                )
            },
            undriven_rows,
            true,
            "an open PR with nobody driving it; report only, never close or defer one - that judgment is the operator's".to_string(),
            "/fno:target",
            None,
        ),
        queue(
            "mergeable_pr",
            format!("{SRC_PRS} + {SRC_PR_GATE}"),
            &inputs.prs,
            pr_rows,
            inputs.autonomous_merge,
            if inputs.autonomous_merge {
                String::new()
            } else {
                "report-only: merging is outward and hard to reverse, so it waits on config.king.autonomous_merge".to_string()
            },
            "",
            Some(mergeable_count),
        ),
        queue(
            "stale_claim",
            format!("{SRC_CLAIMS} + holder_activity"),
            &if inputs.claims.is_ok() && inputs.holder_activity_error.is_none() {
                SourceRead::ok(Value::Null)
            } else {
                SourceRead::err(
                    inputs
                        .claims
                        .error
                        .clone()
                        .or_else(|| inputs.holder_activity_error.clone())
                        .unwrap_or_default(),
                )
            },
            stale_claim_rows,
            true,
            String::new(),
            "",
            None,
        ),
        queue(
            "operator_question",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            question_rows,
            false,
            "report-only: a human answers these, so counting them would hold the loop open forever".to_string(),
            "",
            None,
        ),
        queue(
            "carveout_pending",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            carveout_rows,
            false,
            if carveout_root.is_empty() {
                "report-only: the sweep is a human verb".to_string()
            } else {
                format!("report-only: the sweep is a human verb; root {carveout_root}")
            },
            "",
            Some(as_int(carveout_stream.get("total").unwrap_or(&Value::Null))),
        ),
        queue(
            "capture_pending",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            capture_rows,
            false,
            "report-only: per-project counts only; the rows cannot be listed".to_string(),
            "",
            Some(as_int(capture_stream.get("total").unwrap_or(&Value::Null))),
        ),
        queue(
            "failed_verdict",
            SRC_QUESTIONS.to_string(),
            &inputs.outstanding,
            verdict_rows,
            true,
            "a merged or open node's plan artifacts hold a prove-it FAIL with no ruling; rule with fno inbox decide <node> naming the report, or re-run /fno:review prove-it".to_string(),
            "fno inbox outstanding",
            None,
        ),
        queue(
            "unreachable_worker",
            SRC_NEEDS.to_string(),
            &inputs.needs,
            needs_rows,
            false,
            "report-only: the refusal event a king would act on does not exist yet".to_string(),
            "",
            None,
        ),
    ];

    if let Some(scope) = &inputs.crown_scope {
        if scope_ids.is_some() {
            queues.push(queue(
                "out_of_scope",
                format!("king manifest scope {scope}"),
                &SourceRead::ok(Value::Array(out_of_scope.clone())),
                out_of_scope.clone(),
                false,
                format!("report-only: outside crown scope {scope}"),
                "",
                None,
            ));
        }
    }

    // The scoped board just dropped every row it could not attribute.
    // Name the hole in one warning line, the same shape the holder-activity
    // warning uses, so a quietly shorter queue reads as a measured drop.
    let unattributed = unattributed.into_inner();
    if !unattributed.is_empty() {
        let mut names = unattributed.clone();
        names.sort();
        names.dedup();
        warnings.push(format!(
            "unattributed: {} row(s) with no node binding dropped from this scoped board ({})",
            unattributed.len(),
            names.join(", ")
        ));
    }

    let mut unreadable: i64 = 0;
    let mut over_budget: i64 = 0;
    for q in &queues {
        if not_read_status(q.status) {
            if q.status == "over_budget" {
                over_budget += 1;
            } else {
                unreadable += 1;
            }
        }
    }
    let actionable = actionable_tally(&queues, &mut warnings);

    let queues_json: Vec<Value> = queues.iter().map(queue_json).collect();
    json!({
        "actionable": actionable,
        "unreadable": unreadable,
        "over_budget": over_budget,
        "queues": queues_json,
        "warnings": warnings,
        "exit_code": if unreadable + over_budget > 0 { 1 } else { 0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal board: every source reads empty except the two PR streams.
    /// Scope narrowed to `x-in` so the cross-territory PR exercises AC8.
    fn pr_board_inputs(prs: Value, pr_nodes: Value) -> BoardInputs {
        let empty = SourceRead::ok(json!([]));
        BoardInputs {
            ready: empty.clone(),
            claims: empty.clone(),
            claimed_nodes: empty.clone(),
            holder_activity: HashMap::new(),
            drivers: SourceRead::ok(json!([])),
            holder_activity_error: None,
            prs: SourceRead::ok(prs),
            pr_nodes: SourceRead::ok(pr_nodes),
            pr_gates: SourceRead::ok(json!([])),
            outstanding: empty.clone(),
            needs: empty.clone(),
            lane: empty.clone(),
            undispatched: SourceRead::ok(json!([])),
            blocked_child: empty.clone(),
            worked: empty,
            entries: None,
            held: Default::default(),
            warnings: Vec::new(),
            autonomous_merge: true,
            scope_ids: Some(HashSet::from(["x-in".to_string()])),
            crown_scope: Some("x-in".to_string()),
        }
    }

    fn queue_rows(board: &Value, name: &str) -> Vec<Value> {
        board["queues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["name"] == name)
            .map(|q| q["rows"].as_array().unwrap().clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_cross_territory_mergeable_pr_stays_visible_as_out_of_scope() {
        let inputs = pr_board_inputs(
            json!([
                {"number": 101, "title": "in-scope PR"},
                {"number": 202, "title": "cross-territory review nomination"},
            ]),
            json!([
                {"id": "x-in", "pr_number": 101},
                {"id": "x-out", "pr_number": 202},
            ]),
        );

        let board = build_board(&inputs);

        // AC7: the scoped king's actionable mergeable queue holds only its own PR.
        let mergeable = queue_rows(&board, "mergeable_pr");
        assert_eq!(mergeable.len(), 1);
        assert_eq!(mergeable[0]["number"], 101);
        // AC8: the nomination is demoted, never hidden - the row stays
        // visible with its queue and id, so the review lane can reach it.
        let out = queue_rows(&board, "out_of_scope");
        assert!(
            out.iter()
                .any(|r| r["id"] == "x-out" && r["queue"] == "mergeable_pr"),
            "cross-territory nomination vanished from the board: {out:?}"
        );
    }

    #[test]
    fn a_review_hold_renders_its_blockers_and_reads_not_actionable() {
        // the listing called the PR green and mergeable; the gate
        // holds it. The row stays visible, names its blockers, and the
        // termination reader gets no next-action from it.
        let mut inputs = pr_board_inputs(
            json!([
                {"number": 1709, "title": "green but held"},
            ]),
            json!([{"id": "x-in", "pr_number": 1709}]),
        );
        inputs.pr_gates = SourceRead::ok(json!([
            {"number": 1709, "ready": false,
             "ready_blockers": ["review_in_flight", "review_coverage_uncovered"]},
        ]));

        let board = build_board(&inputs);

        let mergeable = queue_rows(&board, "mergeable_pr");
        assert_eq!(
            mergeable.len(),
            1,
            "the row must stay visible: {mergeable:?}"
        );
        assert_eq!(mergeable[0]["ready"], json!(false));
        assert_eq!(
            mergeable[0]["ready_blockers"],
            json!(["review_in_flight", "review_coverage_uncovered"])
        );
        assert_eq!(mergeable[0]["actionable"], json!(false));
        let q = board["queues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["name"] == "mergeable_pr")
            .unwrap();
        assert_eq!(
            q["count"],
            json!(0),
            "not-ready rows are not actionable: {q}"
        );
    }

    #[test]
    fn a_pr_without_a_gate_verdict_is_not_actionable() {
        // Fail closed: absent verdict means unknown, and unknown is never
        // offered as a merge.
        let inputs = pr_board_inputs(
            json!([
                {"number": 1711, "title": "gate never answered"},
            ]),
            json!([{"id": "x-in", "pr_number": 1711}]),
        );

        let board = build_board(&inputs);

        let mergeable = queue_rows(&board, "mergeable_pr");
        assert_eq!(mergeable.len(), 1, "the row stays visible: {mergeable:?}");
        assert_eq!(mergeable[0]["ready"], Value::Null);
        assert_eq!(mergeable[0]["actionable"], json!(false));
        let q = board["queues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["name"] == "mergeable_pr")
            .unwrap();
        assert_eq!(q["count"], json!(0), "{q}");
    }

    #[test]
    fn a_gate_ready_pr_stays_actionable_mergeable() {
        let mut inputs = pr_board_inputs(
            json!([
                {"number": 101, "title": "actually ready"},
            ]),
            json!([{"id": "x-in", "pr_number": 101}]),
        );
        inputs.pr_gates = SourceRead::ok(json!([
            {"number": 101, "ready": true, "ready_blockers": []},
        ]));

        let board = build_board(&inputs);

        let mergeable = queue_rows(&board, "mergeable_pr");
        assert_eq!(mergeable.len(), 1);
        assert_eq!(mergeable[0]["ready"], json!(true));
        assert_eq!(mergeable[0]["actionable"], json!(true));
        let q = board["queues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["name"] == "mergeable_pr")
            .unwrap();
        assert_eq!(q["count"], json!(1), "{q}");
    }

    #[test]
    fn a_budget_kill_reads_over_budget_not_unreadable() {
        let read = SourceRead::over_budget(
            "fno backlog undispatched --json: killed at its 28.5s slice of the board budget; the source did not fail",
        );
        let q = queue(
            "undispatched",
            "src".to_string(),
            &read,
            Vec::new(),
            true,
            String::new(),
            "/fno:target",
            None,
        );
        assert_eq!(q.status, "over_budget");
        let body = queue_json(&q);
        assert_eq!(body["count"], Value::Null);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("slice of the board budget"));
    }

    #[test]
    fn a_failed_exit_still_reads_unreadable() {
        let read = SourceRead::err("exit 1: boom");
        let q = queue(
            "claims",
            "src".to_string(),
            &read,
            Vec::new(),
            true,
            String::new(),
            "",
            None,
        );
        assert_eq!(q.status, "unreadable");
        let body = queue_json(&q);
        assert_eq!(body["count"], Value::Null);
        assert!(body["error"].as_str().unwrap().contains("exit 1"));
    }

    #[test]
    fn an_open_prove_it_fail_becomes_a_failed_verdict_queue_row() {
        let outstanding = json!({
            "verdicts": {"total": 1, "error": null, "items": [{
                "node": "x-aaaa",
                "report": "/plans/a.md.artifacts/coverage/REPORT.md",
                "verdict": "FAIL",
                "claim": "the probe rejects incomplete evidence",
                "status": "done",
                "mtime": "2026-09-08T20:56:56Z",
            }]}
        });
        let rows = verdict_rows_from(&outstanding);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["node"], "x-aaaa");
        let q = queue(
            "failed_verdict",
            "src".to_string(),
            &SourceRead::ok(outstanding),
            rows,
            true,
            String::new(),
            "fno inbox outstanding",
            None,
        );
        assert_eq!(q.count, 1);
        assert_eq!(queue_json(&q)["count"], 1);
        // No open rows, no queue row: silence is the steady state.
        assert!(verdict_rows_from(&json!({})).is_empty());
        assert!(verdict_rows_from(&json!({"verdicts": {"items": []}})).is_empty());
    }

    #[test]
    fn lane_parser_carries_node_and_parked_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("my-priorities.md");
        std::fs::write(
            &lane,
            "- [ ] ship the board -> x-bbbb\n- [ ] park me -> parked: waiting\n- [x] done item\n- [ ] open item\nnot an item\n",
        )
        .unwrap();
        let items = parse_lane(&lane).unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].node.as_deref(), Some("x-bbbb"));
        assert_eq!(items[0].text, "ship the board");
        assert_eq!(items[1].parked.as_deref(), Some("waiting"));
        assert!(items[2].done);
        assert!(items[3].node.is_none() && items[3].parked.is_none() && !items[3].done);
    }

    #[test]
    fn a_missing_lane_file_is_an_empty_lane_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let items = parse_lane(&dir.path().join("absent.md")).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn a_missing_journal_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let rows = read_blocked_rows(&dir.path().join("absent.jsonl")).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn read_blocked_rows_reads_the_x_eb79_specimen_and_skips_other_types() {
        // The Verification section's own replay: write a blocked row for
        // x-cccc into a fixture journal, read it back by name.
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        std::fs::write(
            &journal,
            format!(
                "{}\n{}\n",
                json!({
                    "ts": "2026-09-08T00:00:00Z", "v": 1, "type": "blocked",
                    "source": "target", "run": "cx-eb79-run", "node": "x-cccc",
                    "data": {"reason": "worktree-init-blocked", "evidence": "Operation not permitted"},
                }),
                json!({"ts": "2026-09-08T00:01:00Z", "type": "other", "run": "cx-eb79-run"}),
            ),
        )
        .unwrap();
        let rows = read_blocked_rows(&journal).unwrap();
        assert_eq!(rows.len(), 1, "the non-blocked row must not appear");
        assert_eq!(rows[0].session, "cx-eb79-run");
        assert_eq!(rows[0].node.as_deref(), Some("x-cccc"));
        assert_eq!(rows[0].reason, "worktree-init-blocked");
        assert_eq!(rows[0].evidence.as_deref(), Some("Operation not permitted"));
    }

    fn blocked(ts: &str, session: &str, node: &str) -> BlockedRow {
        BlockedRow {
            ts: ts.to_string(),
            session: session.to_string(),
            node: Some(node.to_string()),
            reason: "stuck".to_string(),
            evidence: None,
        }
    }

    #[test]
    fn a_closed_node_answers_without_a_mail_check() {
        let rows = vec![blocked("2026-09-08T00:00:00Z", "cx-1", "x-closed")];
        let mut status = HashMap::new();
        status.insert("x-closed".to_string(), "done".to_string());
        let candidates =
            resolve_blocked_child_candidates(rows, &HashMap::new(), &status, 30, 10_000_000_000);
        assert!(
            candidates.is_empty(),
            "a done node must not need a mail spawn"
        );
    }

    #[test]
    fn a_stale_claim_answers_by_release_without_a_mail_check() {
        let rows = vec![blocked("2026-09-08T00:00:00Z", "cx-1", "x-released")];
        let mut claims = HashMap::new();
        claims.insert("x-released".to_string(), "stale".to_string());
        let candidates =
            resolve_blocked_child_candidates(rows, &claims, &HashMap::new(), 30, 10_000_000_000);
        assert!(
            candidates.is_empty(),
            "a released claim must not need a mail spawn"
        );
    }

    #[test]
    fn a_live_claim_inside_grace_is_not_yet_a_candidate() {
        // ts is 10 minutes before now_s; grace is 30 minutes.
        let rows = vec![blocked("2026-09-08T00:00:00Z", "cx-1", "x-live")];
        let mut claims = HashMap::new();
        claims.insert("x-live".to_string(), "live".to_string());
        let row_epoch =
            crate::tick_ledger::parse_rfc3339_unix("2026-09-08T00:00:00Z").unwrap() as i64;
        let candidates =
            resolve_blocked_child_candidates(rows, &claims, &HashMap::new(), 30, row_epoch + 600);
        assert!(
            candidates.is_empty(),
            "10 minutes old must not clear a 30-minute grace"
        );
    }

    #[test]
    fn a_live_claim_past_grace_is_a_candidate_for_the_mail_check() {
        let rows = vec![blocked("2026-09-08T00:00:00Z", "cx-1", "x-live")];
        let mut claims = HashMap::new();
        claims.insert("x-live".to_string(), "live".to_string());
        let row_epoch =
            crate::tick_ledger::parse_rfc3339_unix("2026-09-08T00:00:00Z").unwrap() as i64;
        let candidates = resolve_blocked_child_candidates(
            rows,
            &claims,
            &HashMap::new(),
            30,
            row_epoch + 45 * 60,
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1, 45);
    }

    #[test]
    fn oldest_row_wins_when_a_session_carries_two_distinct_reasons() {
        let rows = vec![
            blocked("2026-09-08T01:00:00Z", "cx-1", "x-a"),
            blocked("2026-09-08T00:00:00Z", "cx-1", "x-a"),
        ];
        let mut claims = HashMap::new();
        claims.insert("x-a".to_string(), "live".to_string());
        let now = crate::tick_ledger::parse_rfc3339_unix("2026-09-08T02:00:00Z").unwrap() as i64;
        let candidates = resolve_blocked_child_candidates(rows, &claims, &HashMap::new(), 30, now);
        assert_eq!(
            candidates.len(),
            1,
            "one row per session, never one per reason"
        );
        assert_eq!(
            candidates[0].0.ts, "2026-09-08T00:00:00Z",
            "the OLDEST row wins"
        );
    }

    #[test]
    fn ac3_edge_mail_after_the_row_clears_the_candidate() {
        let row = blocked("2026-09-08T00:00:00Z", "cx-1", "x-a");
        let mut answered = HashMap::new();
        answered.insert("cx-1".to_string(), true);
        let out = filter_unanswered_by_mail(vec![(row, 45)], &answered, &HashMap::new());
        assert!(
            out.is_empty(),
            "a session with mail after its row must not appear"
        );
    }

    #[test]
    fn ac3_edge_no_answer_still_names_the_row() {
        let row = blocked("2026-09-08T00:00:00Z", "cx-1", "x-a");
        let mut verdicts = HashMap::new();
        verdicts.insert("cx-1".to_string(), "ghost".to_string());
        let out = filter_unanswered_by_mail(vec![(row, 45)], &HashMap::new(), &verdicts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "x-a");
        assert_eq!(out[0]["session"], "cx-1");
        assert_eq!(out[0]["age_minutes"], 45);
        assert_eq!(
            out[0]["watchdog_verdict"], "ghost",
            "the queue surfaces the watchdog's own verdict rather than judging staleness itself"
        );
    }

    /// Bus fixture for the answered-row tests: the given bus lines into a
    /// tempdir `messages.jsonl`, as the measured specimen lines land on disk.
    fn bus_with(lines: &[Value]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("messages.jsonl");
        let text: String = lines.iter().map(|v| format!("{}\n", v)).collect();
        std::fs::write(&live, text).unwrap();
        (dir, live)
    }

    #[test]
    fn mail_answered_since_reads_a_rotated_segment_not_just_the_live_file() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("messages.jsonl");
        std::fs::write(
            live.with_extension("jsonl.1"),
            format!("{}\n", json!({"to": "cx-1", "ts": "2026-09-08T21:00:00Z"})),
        )
        .unwrap();
        std::fs::write(&live, "not json\n").unwrap();
        let candidates = vec![("cx-1".to_string(), None, "2026-09-08T20:00:00Z".to_string())];
        let answered = mail_answered_since(&live, &candidates);
        assert_eq!(answered.get("cx-1"), Some(&true));
    }

    #[test]
    fn mail_answered_since_a_missing_bus_reads_unanswered_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = vec![("cx-1".to_string(), None, "2026-09-08T20:00:00Z".to_string())];
        let answered = mail_answered_since(&dir.path().join("messages.jsonl"), &candidates);
        assert_eq!(answered.get("cx-1"), Some(&false));
    }

    #[test]
    fn a_run_keyed_row_is_answered_by_mail_to_the_holder_handle() {
        let (_dir, live) = bus_with(&[json!({
            "from": "278c9a89", "to": "77393822", "ts": "2026-09-15T07:28:11Z"
        })]);
        let candidates = vec![(
            "20260915T033130Z-cl38242-b8e631".to_string(),
            Some("77393822-9c90-4fa9-bdb1-def84b6178f8".to_string()),
            "2026-09-15T06:31:48Z".to_string(),
        )];
        let answered = mail_answered_since(&live, &candidates);
        assert_eq!(answered.get("20260915T033130Z-cl38242-b8e631"), Some(&true));
    }

    #[test]
    fn a_self_send_to_the_holder_does_not_answer() {
        let (_dir, live) = bus_with(&[
            json!({"from": "77393822", "to": "77393822", "ts": "2026-09-15T11:18:17Z"}),
            json!({"from": "77393822", "to": "77393822", "ts": "2026-09-15T11:38:33Z"}),
        ]);
        let candidates = vec![(
            "20260915T033130Z-cl38242-b8e631".to_string(),
            Some("77393822-9c90-4fa9-bdb1-def84b6178f8".to_string()),
            "2026-09-15T06:31:48Z".to_string(),
        )];
        let answered = mail_answered_since(&live, &candidates);
        assert_eq!(
            answered.get("20260915T033130Z-cl38242-b8e631"),
            Some(&false)
        );
    }

    #[test]
    fn mail_to_the_holder_before_the_help_does_not_answer() {
        let (_dir, live) = bus_with(&[json!({
            "from": "278c9a89", "to": "77393822", "ts": "2026-09-15T05:30:11Z"
        })]);
        let candidates = vec![(
            "20260915T033130Z-cl38242-b8e631".to_string(),
            Some("77393822-9c90-4fa9-bdb1-def84b6178f8".to_string()),
            "2026-09-15T06:31:48Z".to_string(),
        )];
        let answered = mail_answered_since(&live, &candidates);
        assert_eq!(
            answered.get("20260915T033130Z-cl38242-b8e631"),
            Some(&false)
        );
    }

    #[test]
    fn a_harness_keyed_row_is_answered_at_its_first_eight() {
        let (_dir, live) = bus_with(&[json!({
            "from": "278c9a89", "to": "77393822", "ts": "2026-09-15T07:28:11Z"
        })]);
        let candidates = vec![(
            "77393822-9c90-4fa9-bdb1-def84b6178f8".to_string(),
            None,
            "2026-09-15T06:31:48Z".to_string(),
        )];
        let answered = mail_answered_since(&live, &candidates);
        assert_eq!(
            answered.get("77393822-9c90-4fa9-bdb1-def84b6178f8"),
            Some(&true)
        );
    }

    #[test]
    fn a_run_keyed_row_with_no_holder_answers_only_at_its_run_id() {
        let (_dir, live) = bus_with(&[
            json!({"from": "278c9a89", "to": "77393822", "ts": "2026-09-15T07:28:11Z"}),
            json!({"from": "fno", "to": "20260915", "ts": "2026-09-15T08:00:00Z"}),
            json!({"from": "278c9a89", "to": "20260915T033130Z-cl38242-b8e631", "ts": "2026-09-15T09:00:00Z"}),
        ]);
        let candidates = vec![(
            "20260915T033130Z-cl38242-b8e631".to_string(),
            None,
            "2026-09-15T06:31:48Z".to_string(),
        )];
        let answered = mail_answered_since(&live, &candidates);
        assert_eq!(answered.get("20260915T033130Z-cl38242-b8e631"), Some(&true));
    }

    #[test]
    fn a_run_id_never_matches_by_its_first_eight() {
        assert!(!addresses_row(
            "20260915",
            "20260915T033130Z-cl38242-b8e631",
            None
        ));
    }

    #[test]
    fn verdict_for_falls_back_to_the_holder_session() {
        let payload = json!({"77393822-9c90-4fa9-bdb1-def84b6178f8": "ghost"});
        let verdict = verdict_for(
            Some(&payload),
            "20260915T033130Z-cl38242-b8e631",
            Some("77393822-9c90-4fa9-bdb1-def84b6178f8"),
        );
        assert_eq!(verdict.as_deref(), Some("ghost"));
        let no_holder = verdict_for(Some(&payload), "20260915T033130Z-cl38242-b8e631", None);
        assert_eq!(no_holder, None);
    }
}
