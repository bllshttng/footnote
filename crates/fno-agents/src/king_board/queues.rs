//! The operator lane parser and the thirteen-queue board build (pure; no I/O).
use super::classify::{claim_is_dead, holder_token, node_driver, node_has_pr};
use super::prs::derived_status;
use super::scope::operator_lane_path;
use super::{
    as_int, s_str, truthy, SourceRead, DEAD_CLAIM_STATES, KING_PRIORITIES, LEGACY_DEFER_PREFIX,
    SRC_CLAIMS, SRC_DISTRESS, SRC_NEEDS, SRC_PRS, SRC_PR_NODES, SRC_QUESTIONS, SRC_READY,
    SRC_UNDISPATCHED, SRC_WORKED, TERMINAL_RUNGS,
};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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

/// `to == session && ts > cutoff` across the live bus log plus its rotated
/// `.N` segments, oldest first - the mail-answered signal AC3-EDGE names.
/// Read-only and mechanical (no rotation/locking, the writer's job), so it
/// stays a native Rust read rather than a Python subprocess per Change 1's
/// own file list.
pub(crate) fn mail_answered_since(
    live_log: &Path,
    cutoffs: &HashMap<String, String>,
) -> HashMap<String, bool> {
    let mut answered: HashMap<String, bool> = cutoffs.keys().map(|s| (s.clone(), false)).collect();
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
            if cutoffs.get(to).is_some_and(|cutoff| ts > cutoff.as_str()) {
                answered.insert(to.to_string(), true);
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
    pub(crate) holder_activity: HashMap<String, crate::truth_probe::TruthProbe>,
    /// The truth batch's failure receipt: `Some` when the batch timed out or
    /// its reader panicked. The claim-dependent queues read unreadable
    /// rather than rendering an absent measurement as a verdict (x-db9c).
    pub(crate) holder_activity_error: Option<String>,
    pub(crate) prs: SourceRead,
    pub(crate) pr_nodes: SourceRead,
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
    pub(crate) warnings: Vec<String>,
    pub(crate) autonomous_merge: bool,
    pub(crate) scope_ids: Option<HashSet<String>>,
    pub(crate) crown_scope: Option<String>,
}

/// Build the board payload. Pure; does no I/O. Queue names, order, and row
/// shapes match board.py's `build_board` exactly.
pub(crate) fn build_board(inputs: &BoardInputs) -> Value {
    let mut warnings = inputs.warnings.clone();
    let mut out_of_scope: Vec<Value> = Vec::new();
    // x-2fde: on a scoped board a row whose node id cannot be resolved is
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

    // x-db9c: a holder the probe batch never answered for is a hole in the
    // board's evidence, not a worker verdict. Name every hole in one warning
    // line so a partially-answered batch is visible in the payload, not only
    // through the rows its absence silently removed. The expected set mirrors
    // the probe feed exactly (king-priority claimed nodes + dead-state
    // claims): a live claim on a lower-priority node is never fed to the
    // probe, so counting it here would warn forever about a holder nobody
    // promised to measure.
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
            // x-db9c: the ready feed drops claimed nodes it cannot see
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
            let node_id = s_str(row, "key")
                .and_then(|k| k.strip_prefix("node:"))
                .unwrap_or("");
            in_scope(
                "stale_claim",
                false,
                &json!(node_id),
                row,
                &mut out_of_scope,
            )
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
            );
            if state != "none" {
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
        .map(|r| json!({"number": r.get("number"), "title": r.get("title")}))
        .collect();

    // Undriven PR: the complement of stalled_holder, the second half of ONE
    // predicate. Fail CLOSED on an unreadable claim list: every node would
    // read "none" and the king would dispatch over every live worker at once.
    let mergeable_numbers: HashSet<i64> = if inputs.autonomous_merge {
        pr_rows
            .iter()
            .filter_map(|r| r.get("number").and_then(Value::as_i64))
            .collect()
    } else {
        HashSet::new()
    };
    let mut undriven_rows: Vec<Value> = Vec::new();
    // x-dead task 1.4b: the pr_nodes rows carry no `contained_in` (the field
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
    if inputs.pr_nodes.is_ok() && inputs.claims.is_ok() {
        for node in &inputs.pr_nodes.rows() {
            if !KING_PRIORITIES.contains(&s_str(node, "priority").unwrap_or("")) {
                continue;
            }
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
            let (state, _claim) = node_driver(
                node,
                &claim_by_node,
                &inputs.holder_activity,
                inputs.scope_ids.as_ref(),
                Some(&inputs.worked),
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
            format!("{SRC_READY} + {SRC_CLAIMS} + {SRC_WORKED} + holder_activity"),
            &if inputs.ready.is_ok()
                && inputs.claims.is_ok()
                && inputs.worked.is_ok()
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
            format!("{SRC_CLAIMS} + fno backlog get <id> + fno agents peek <worker>"),
            &if inputs.claims.is_ok()
                && inputs.claimed_nodes.is_ok()
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
            format!("graph entries + {SRC_CLAIMS} + holder_activity"),
            &if inputs.entries.is_some()
                && inputs.claims.is_ok()
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
            format!("{SRC_PR_NODES} + {SRC_CLAIMS} + holder_activity"),
            &if inputs.pr_nodes.is_ok()
                && inputs.claims.is_ok()
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
            SRC_PRS.to_string(),
            &inputs.prs,
            pr_rows,
            inputs.autonomous_merge,
            if inputs.autonomous_merge {
                String::new()
            } else {
                "report-only: merging is outward and hard to reverse, so it waits on config.king.autonomous_merge".to_string()
            },
            "",
            None,
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

    // x-2fde: the scoped board just dropped every row it could not attribute.
    // Name the hole in one warning line (the x-db9c shape) so a quietly
    // shorter queue reads as a measured drop, not as an empty world.
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

    let mut actionable: i64 = 0;
    let mut unreadable: i64 = 0;
    let mut over_budget: i64 = 0;
    for q in &queues {
        if not_read_status(q.status) {
            if q.status == "over_budget" {
                over_budget += 1;
            } else {
                unreadable += 1;
            }
            // A blind ACTIONABLE queue is work: the king may not exit while it
            // cannot see a queue it could have shrunk. A blind report-only
            // queue is loud (the exit code) and still uncounted.
            if q.actionable {
                actionable += 1;
            }
        } else if q.actionable {
            actionable += q.count;
        }
    }

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
    fn lane_parser_carries_node_and_parked_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("my-priorities.md");
        std::fs::write(
            &lane,
            "- [ ] ship the board -> x-25b8\n- [ ] park me -> parked: waiting\n- [x] done item\n- [ ] open item\nnot an item\n",
        )
        .unwrap();
        let items = parse_lane(&lane).unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].node.as_deref(), Some("x-25b8"));
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
        // x-eb79 into a fixture journal, read it back by name.
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        std::fs::write(
            &journal,
            format!(
                "{}\n{}\n",
                json!({
                    "ts": "2026-09-08T00:00:00Z", "v": 1, "type": "blocked",
                    "source": "target", "run": "cx-eb79-run", "node": "x-eb79",
                    "data": {"reason": "worktree-init-blocked", "evidence": "Operation not permitted"},
                }),
                json!({"ts": "2026-09-08T00:01:00Z", "type": "other", "run": "cx-eb79-run"}),
            ),
        )
        .unwrap();
        let rows = read_blocked_rows(&journal).unwrap();
        assert_eq!(rows.len(), 1, "the non-blocked row must not appear");
        assert_eq!(rows[0].session, "cx-eb79-run");
        assert_eq!(rows[0].node.as_deref(), Some("x-eb79"));
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
        let mut cutoffs = HashMap::new();
        cutoffs.insert("cx-1".to_string(), "2026-09-08T20:00:00Z".to_string());
        let answered = mail_answered_since(&live, &cutoffs);
        assert_eq!(answered.get("cx-1"), Some(&true));
    }

    #[test]
    fn mail_answered_since_a_missing_bus_reads_unanswered_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut cutoffs = HashMap::new();
        cutoffs.insert("cx-1".to_string(), "2026-09-08T20:00:00Z".to_string());
        let answered = mail_answered_since(&dir.path().join("messages.jsonl"), &cutoffs);
        assert_eq!(answered.get("cx-1"), Some(&false));
    }
}
