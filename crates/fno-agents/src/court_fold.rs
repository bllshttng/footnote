//! `court-fold`: the crown scope fold for `fno agents court --nodes` and the
//! local board's court section (x-52d2).
//!
//! Python passes the crowns `gather_court` already adjudicated; this verb
//! reads graph.json and the claims dir directly (the same files the keeper
//! serves, one direct read instead of a second keeper round-trip), compiles
//! each crown's scope with the same rules `king_board/scope.rs` applies, and
//! returns the per-scope fold as JSON or as the board's HTML section. The
//! worker column names live/suspect claim holders through the same native
//! verdict machinery `claim sweep` uses, so the two surfaces cannot disagree
//! about who holds a node.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

/// The statuses a reader means by "what is being worked on" (see
/// docs/architecture/court-scope-fold.md): neither closed (done, superseded)
/// nor unstarted (idea, deferred).
pub(crate) const ACTIVE_STATUSES: [&str; 5] =
    ["in_progress", "in_review", "ready", "blocked", "design"];

/// Counts render in lifecycle order; a status outside the vocabulary keeps
/// its place at the end rather than vanishing from the line.
pub(crate) const COUNT_ORDER: [&str; 9] = [
    "in_progress",
    "in_review",
    "ready",
    "blocked",
    "design",
    "idea",
    "deferred",
    "done",
    "superseded",
];

/// How long a node may sit ready or unclaimed before the court calls it stuck.
/// One threshold, one default, and no knob until somebody asks for a different
/// number.
const STUCK_AFTER_MINUTES: f64 = 60.0;

/// The claim verdicts that mean somebody holds the node. `suspect` is a
/// respawned worker whose supervisor pid died: the TTL still protects it, so it
/// is held, not free.
const HELD_CLAIMS: [&str; 2] = ["live", "suspect"];

/// The claim verdicts that prove nothing either way. An unproven claim blocks a
/// dispatch as hard as a held one does, so it is stuck rather than free.
const UNPROVEN_CLAIMS: [&str; 2] = ["corrupted", "unreadable"];

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// html.escape(quote=True) semantics: the section lands in a document the
/// scripted board also writes into, and the same bytes must escape the same
/// way on both sides.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

/// Ordered de-duplicated union of the four session fields a node can carry:
/// sessions[].session_id, then session_id, then cost_sessions, then
/// locked_by_harness_session. First occurrence wins.
fn sessions_of(entry: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |sid: Option<String>| {
        if let Some(sid) = sid {
            if !sid.is_empty() && !out.contains(&sid) {
                out.push(sid);
            }
        }
    };
    if let Some(list) = entry.get("sessions").and_then(|v| v.as_array()) {
        for raw in list {
            let sid = match raw {
                Value::Object(_) => raw
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                Value::String(s) => Some(s.clone()),
                _ => None,
            };
            push(sid);
        }
    }
    push(
        entry
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    );
    if let Some(list) = entry.get("cost_sessions").and_then(|v| v.as_array()) {
        for raw in list {
            // Ledger-derived cost sessions arrive as objects carrying
            // session_id; older graphs hold bare strings.
            let sid = match raw {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => raw
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                _ => None,
            };
            push(sid);
        }
    }
    push(
        entry
            .get("locked_by_harness_session")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    );
    out
}

/// Compile a crown's scope to its node ids at the crown's own level - the
/// injected-resolver arm of Python `compile_scope_ids`: the level comes from
/// the crown row `gather_court` already adjudicated, never re-resolved from
/// config (a row reading level=2 over a project must fold as epics and fail,
/// not silently re-resolve into the project's nodes).
pub(crate) fn compile_forced(
    scope: &str,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
    level: i64,
) -> Result<BTreeSet<String>, String> {
    let entry_by_id = |id: &str| {
        entries
            .iter()
            .find(|e| s_str(e, "id").map(|i| i == id).unwrap_or(false))
    };
    let members: Vec<String> = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        return Err("a crown needs a scope: name an epic or a project".to_string());
    }
    let mut ids = BTreeSet::new();
    if level == 2 {
        for root_id in &members {
            match entry_by_id(root_id) {
                None => {
                    return Err(format!(
                        "crown scope {root_id:?} is not an epic in the graph"
                    ))
                }
                Some(entry) if s_str(entry, "type") != Some("epic") => {
                    return Err(format!(
                        "crown scope {root_id:?} is not an epic in the graph"
                    ));
                }
                Some(_) => {}
            }
        }
        // descendants_of: BFS over parent links, cycle-safe; a rung-2 scope is
        // a SET, so the walk starts from every member.
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in entries {
            if let (Some(id), Some(parent)) = (s_str(e, "id"), s_str(e, "parent")) {
                children.entry(parent).or_default().push(id);
            }
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut frontier: Vec<&str> = members.iter().map(|s| s.as_str()).collect();
        while let Some(id) = frontier.pop() {
            if !seen.insert(id) {
                continue;
            }
            ids.insert(id.to_string());
            if let Some(kids) = children.get(id) {
                for kid in kids {
                    if !seen.contains(kid) {
                        frontier.push(kid);
                    }
                }
            }
        }
        return Ok(ids);
    }
    // Rung 0/1: every node whose canonical project matches - no epic
    // containment required. The entry side canonicalizes (short name ->
    // canonical) or falls back to the raw field, exactly as the Python fold
    // reads `_canonical_project(p) or p`.
    let crown_projects: std::collections::HashSet<String> = members.iter().cloned().collect();
    let map = projects.clone().unwrap_or_default();
    for e in entries {
        let Some(id) = s_str(e, "id") else { continue };
        let raw_project = s_str(e, "project").unwrap_or("");
        let canonical = map
            .get(raw_project)
            .cloned()
            .unwrap_or_else(|| raw_project.to_string());
        if crown_projects.contains(&canonical) {
            ids.insert(id.to_string());
        }
    }
    Ok(ids)
}

/// Node id -> the swept claim row for that node, through the same native
/// verdict machinery `claim sweep` uses. Every selected row is kept, not only
/// the held ones: `free`, `stale` and "no record at all" are three different
/// answers and one null renders them as one.
///
/// The bool says whether the sweep reached the store. Any fault degrades to an
/// empty map with that bool false, so a display read never raises and a caller
/// can still tell an absence from a broken instrument. `list_in_result` names
/// the directories whose scan succeeded, which is the same distinction one
/// layer down.
fn live_workers(claims_dir: Option<&PathBuf>, keys: &[String]) -> (BTreeMap<String, Value>, bool) {
    let Some(dir) = claims_dir else {
        return (BTreeMap::new(), false);
    };
    let Ok((records, read_dirs)) =
        crate::claims::list_in_result(std::slice::from_ref(dir), None, true)
    else {
        return (BTreeMap::new(), false);
    };
    if read_dirs.is_empty() {
        return (BTreeMap::new(), false);
    }
    let payload = crate::claim_verbs::claim_sweep_payload_from_records(&records, None, keys, false);
    let mut out = BTreeMap::new();
    if let Some(rows) = payload.get("claims").and_then(|v| v.as_array()) {
        for row in rows {
            let key = s_str(row, "key").unwrap_or_default().to_string();
            out.insert(key.trim_start_matches("node:").to_string(), row.clone());
        }
    }
    (out, true)
}

/// Whole hours since `created_at`, to one decimal. `None` when the entry
/// carries no parseable stamp, which a reader must not read as "brand new".
fn age_hours(entry: &Value, now_secs: u64) -> Value {
    let Some(created) = s_str(entry, "created_at").and_then(crate::tick_ledger::parse_rfc3339_unix)
    else {
        return Value::Null;
    };
    let hours = now_secs.saturating_sub(created) as f64 / 3600.0;
    json!((hours * 10.0).round() / 10.0)
}

/// The fold for one crown: counts over the whole scope, rows for the active
/// statuses only, and `omitted` stated, never implied.
fn fold_one(
    scope: &str,
    level: Option<i64>,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
    workers: &BTreeMap<String, Value>,
    sweep_ran: bool,
    now_secs: u64,
) -> Value {
    let Some(level) = level else {
        return json!({
            "status": "unresolved",
            "reason": "the row carries no scope or no crown level",
        });
    };
    let ids = match compile_forced(scope, entries, projects, level) {
        Ok(ids) => ids,
        Err(reason) => return json!({"status": "unresolved", "reason": reason}),
    };
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    let mut nodes: Vec<Value> = Vec::new();
    for id in &ids {
        let Some(entry) = entries
            .iter()
            .find(|e| s_str(e, "id").map(|i| i == id).unwrap_or(false))
        else {
            continue;
        };
        let status = s_str(entry, "status").unwrap_or("unknown").to_string();
        *counts.entry(status.clone()).or_insert(0) += 1;
        if !ACTIVE_STATUSES.contains(&status.as_str()) {
            continue;
        }
        let sessions = sessions_of(entry);
        let claim = workers.get(id);
        let state = claim.and_then(|c| s_str(c, "state"));
        // An absence and a failed instrument must not print the same string,
        // so the sweep's own verdict is used only when the sweep ran.
        let claim_state = if !sweep_ran {
            "unreadable"
        } else {
            state.unwrap_or("no-record")
        };
        let held = state.is_some_and(|s| HELD_CLAIMS.contains(&s));
        nodes.push(json!({
            "id": id,
            "slug": s_str(entry, "slug").unwrap_or(""),
            "status": status,
            "worker": if held {
                claim.and_then(|c| c.get("holder")).cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            },
            "claim_state": claim_state,
            "claim_basis": claim.and_then(|c| c.get("basis")).cloned().unwrap_or(Value::Null),
            "pr_number": entry.get("pr_number").cloned().unwrap_or(Value::Null),
            "sessions": sessions,
            "age_hours": age_hours(entry, now_secs),
            "blocked_by": entry.get("blocked_by").cloned().unwrap_or(Value::Null),
            "blocked_reason": entry.get("blocked_reason").cloned().unwrap_or(Value::Null),
        }));
    }
    let total: i64 = counts.values().sum();
    let mut ordered = Map::new();
    for key in COUNT_ORDER {
        if let Some(v) = counts.get(key) {
            ordered.insert(key.to_string(), json!(v));
        }
    }
    for (key, v) in &counts {
        if !ordered.contains_key(key) {
            ordered.insert(key.clone(), json!(v));
        }
    }
    json!({
        "status": "ok",
        "total": total,
        "counts": ordered,
        "nodes": nodes,
        "omitted": total - nodes.len() as i64,
    })
}

/// One crown row of the HTML section: the summary carries scope, level,
/// holder and the agree marker; a disagreement gets the marker class and its
/// reason inline; the body is the fold's counts line and active rows.
fn crown_html(crown: &Value, fold: &Value) -> String {
    let agree = crown.get("agree").and_then(|a| a.as_bool());
    let marker = match agree {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    };
    let disagree = agree == Some(false);
    let cls = if disagree { " court-disagree" } else { "" };
    let scope = s_str(crown, "scope").unwrap_or("");
    let level = crown
        .get("level")
        .map(|l| match l {
            Value::Null => "None".to_string(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "None".into());
    let holder = s_str(crown, "holder").unwrap_or("");
    let mut out = format!("<details class=\"crown{cls}\"><summary>");
    out.push_str(&esc(&format!(
        "{scope} · L{level} · {holder} · agree {marker}"
    )));
    if disagree {
        if let Some(reason) = s_str(crown, "reason") {
            out.push_str(&esc(&format!(" - {reason}")));
        }
    }
    out.push_str("</summary>");
    if fold.get("status").and_then(|s| s.as_str()) == Some("unresolved") {
        let reason = s_str(fold, "reason").unwrap_or("");
        out.push_str(&format!(
            "<div class=\"crown-body\"><span class=\"crown-note\">{}</span></div>",
            esc(&format!("scope fold: unresolved - {reason}"))
        ));
        out.push_str("</details>");
        return out;
    }
    let mut counts = String::new();
    if let Some(map) = fold.get("counts").and_then(|c| c.as_object()) {
        let parts: Vec<String> = map.iter().map(|(k, v)| format!("{k} {}", v)).collect();
        counts = parts.join(", ");
    }
    let total = fold.get("total").and_then(|t| t.as_i64()).unwrap_or(0);
    let omitted = fold.get("omitted").and_then(|o| o.as_i64()).unwrap_or(0);
    out.push_str("<div class=\"crown-body\">");
    out.push_str(&format!(
        "<div class=\"crown-counts\">{} nodes: {} ({} not listed)</div>",
        esc(&total.to_string()),
        esc(&counts),
        esc(&omitted.to_string())
    ));
    out.push_str(
        "<table class=\"crown-nodes\"><thead><tr><th>node</th><th>status</th><th>worker</th>\
         <th>claim</th><th>age</th><th>pr</th><th>sessions</th></tr></thead><tbody>",
    );
    if let Some(rows) = fold.get("nodes").and_then(|n| n.as_array()) {
        for r in rows {
            let pr = r
                .get("pr_number")
                .and_then(|p| p.as_i64())
                .map(|n| format!("#{n}"))
                .unwrap_or_default();
            let sessions = r
                .get("sessions")
                .and_then(|s| s.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let age = r
                .get("age_hours")
                .and_then(|a| a.as_f64())
                .map(|h| format!("{h:.1}h"))
                .unwrap_or_default();
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(s_str(r, "id").unwrap_or("")),
                esc(s_str(r, "status").unwrap_or("")),
                esc(s_str(r, "worker").unwrap_or("-")),
                esc(s_str(r, "claim_state").unwrap_or("-")),
                esc(&age),
                esc(&pr),
                esc(&sessions),
            ));
        }
    }
    out.push_str("</tbody></table></div></details>");
    out
}

/// What is stuck across every crown, and what could not be answered.
///
/// The counts say how much. This says whether anything needs a hand, which is
/// the only part of the read worth a glance. It lives here, beside the rows it
/// judges, so no second reader can disagree about what a row means.
///
/// A node is counted ONCE. An L1 crown folds the nodes its L2 epics also fold,
/// so an overlapping node reaches this loop once per crown covering it, and
/// counting it twice would report more stuck work than exists.
fn stuck_verdict(folds: &BTreeMap<String, Value>) -> Value {
    let threshold = STUCK_AFTER_MINUTES / 60.0;
    let mut unclaimed: Vec<String> = Vec::new();
    let mut blocked: Vec<Value> = Vec::new();
    let mut unproven: Vec<String> = Vec::new();
    let mut in_review: Vec<String> = Vec::new();
    let mut blind: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for fold in folds.values() {
        if fold.get("status").and_then(|s| s.as_str()) != Some("ok") {
            // One cause is one line: several crowns failing the same way is one
            // fault, and repeating it buries the verdict.
            let reason = s_str(fold, "reason")
                .unwrap_or("a crown's scope fold did not run")
                .to_string();
            if !blind.contains(&reason) {
                blind.push(reason);
            }
            continue;
        }
        let Some(nodes) = fold.get("nodes").and_then(|n| n.as_array()) else {
            continue;
        };
        for node in nodes {
            let Some(id) = s_str(node, "id") else {
                continue;
            };
            if !seen.insert(id.to_string()) {
                continue;
            }
            let claim = s_str(node, "claim_state").unwrap_or("");
            let unproven_claim = UNPROVEN_CLAIMS.contains(&claim);
            if unproven_claim {
                unproven.push(id.to_string());
            }
            let held = HELD_CLAIMS.contains(&claim);
            let old = node
                .get("age_hours")
                .and_then(|a| a.as_f64())
                .map(|h| h > threshold)
                .unwrap_or(false);
            let has_pr = node.get("pr_number").map(|p| !p.is_null()).unwrap_or(false);
            // `blocked_by` is a graph field, so it is classified whatever the
            // claim store said. An unreadable store must not erase a fact that
            // never came from it. The other two buckets read the claim, so an
            // unproven one is reported there and not counted twice here.
            match s_str(node, "status") {
                // A count that never says what on is the gap this closes.
                Some("blocked") => blocked.push(json!({
                    "id": id,
                    "blocked_by": node.get("blocked_by").cloned().unwrap_or(Value::Null),
                })),
                Some("ready") | Some("in_progress") if !unproven_claim && !held && old => {
                    unclaimed.push(id.to_string())
                }
                Some("in_review") if !unproven_claim && old && has_pr => {
                    in_review.push(id.to_string())
                }
                _ => {}
            }
        }
    }
    json!({
        "unclaimed": unclaimed,
        "blocked": blocked,
        "unproven_claim": unproven,
        "in_review": in_review,
        "blind": blind,
        "threshold_minutes": STUCK_AFTER_MINUTES as i64,
    })
}

/// The node half of the one-line verdict. The caller appends what only it can
/// know, such as the spawn gate's refusal.
///
/// An empty string means the rows answered and nothing was stuck. A blind read
/// never returns empty, because a clean line and a blind line must not look the
/// same.
fn stuck_line(stuck: &Value) -> String {
    /// How many ids a clause names before it counts the rest. The line exists
    /// to be glanced at, and a live court put 40 ids in one clause. The full
    /// list is always in the JSON.
    const NAMED: usize = 5;
    fn named(ids: &[String]) -> String {
        if ids.len() <= NAMED {
            return ids.join(", ");
        }
        format!("{}, +{} more", ids[..NAMED].join(", "), ids.len() - NAMED)
    }
    let ids = |key: &str| -> Vec<String> {
        stuck
            .get(key)
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut parts: Vec<String> = Vec::new();
    let unclaimed = ids("unclaimed");
    if !unclaimed.is_empty() {
        parts.push(format!(
            "{} ready over {}m with no worker ({})",
            unclaimed.len(),
            STUCK_AFTER_MINUTES as i64,
            named(&unclaimed)
        ));
    }
    if let Some(rows) = stuck.get("blocked").and_then(|v| v.as_array()) {
        if !rows.is_empty() {
            let mut on: BTreeSet<String> = BTreeSet::new();
            for row in rows {
                for b in row
                    .get("blocked_by")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(b) = b.as_str() {
                        on.insert(b.to_string());
                    }
                }
            }
            let tail = if on.is_empty() {
                " (on nothing named)".to_string()
            } else {
                format!(" (on {})", named(&on.into_iter().collect::<Vec<_>>()))
            };
            parts.push(format!("{} blocked{tail}", rows.len()));
        }
    }
    let unproven = ids("unproven_claim");
    if !unproven.is_empty() {
        parts.push(format!(
            "{} with an unproven claim ({})",
            unproven.len(),
            named(&unproven)
        ));
    }
    let in_review = ids("in_review");
    if !in_review.is_empty() {
        parts.push(format!(
            "{} in review over {}m ({})",
            in_review.len(),
            STUCK_AFTER_MINUTES as i64,
            named(&in_review)
        ));
    }
    for reason in ids("blind") {
        parts.push(format!("could not answer: {reason}"));
    }
    parts.join(", ")
}

/// The board section: the styles ride with the markup so the fragment is
/// self-contained and `_DASHBOARD_CSS` never needs to know the section
/// exists.
fn section_css() -> &'static str {
    "<style>section.court{margin-top:18px;display:flex;flex-direction:column;gap:6px}\
section.court h2{margin:0;font-size:13px;color:var(--ink-2);text-transform:uppercase;letter-spacing:.04em}\
details.crown{border:1px solid var(--line);border-radius:8px;background:var(--surface)}\
details.crown summary{cursor:pointer;padding:8px 11px;font-size:12.5px;font-family:\"IBM Plex Mono\",ui-monospace,monospace;color:var(--ink-2)}\
details.crown summary::marker{color:var(--muted)}\
details.crown.court-disagree{border-color:var(--blocked)}\
details.crown.court-disagree summary{color:var(--blocked)}\
.crown-body{padding:0 11px 10px}.crown-counts{font-size:11.5px;color:var(--muted);margin-bottom:6px}\
.crown-note{font-size:11.5px;color:var(--blocked)}\
table.crown-nodes{width:100%;border-collapse:collapse;font-size:11.5px}\
table.crown-nodes th{text-align:left;color:var(--muted);font-weight:500;padding:3px 8px 3px 0;border-bottom:1px solid var(--line)}\
table.crown-nodes td{padding:3px 8px 3px 0;border-bottom:1px solid var(--surface-2);vertical-align:top;font-family:\"IBM Plex Mono\",ui-monospace,monospace;word-break:break-all}</style>"
}

/// The whole read: fold every crown, then answer as JSON or as the board
/// section. `format` selects the answer; the fold is identical either way.
pub fn court_fold(
    graph_path: &PathBuf,
    cwd: &PathBuf,
    claims_dir: Option<&PathBuf>,
    crowns: &[Value],
    format: &str,
) -> Result<Value, String> {
    let entries: Vec<Value> = crate::graph_store::read_defaulted_opts(graph_path, false, false)
        .map_err(|e| format!("graph unreadable: {e}"))?;
    let projects = crate::king_board::project_map(cwd);
    let now_secs = (crate::claims::now_ms() / 1000).max(0) as u64;
    // Pass 1: fold with no workers named, collecting the node ids the worker
    // sweep will ask after.
    let mut want: BTreeSet<String> = BTreeSet::new();
    let mut folds: BTreeMap<String, Value> = BTreeMap::new();
    for crown in crowns {
        let Some(scope) = s_str(crown, "scope") else {
            continue;
        };
        let level = crown.get("level").and_then(|l| l.as_i64());
        let fold = fold_one(
            scope,
            level,
            &entries,
            &projects,
            &BTreeMap::new(),
            false,
            now_secs,
        );
        if let Some(nodes) = fold.get("nodes").and_then(|n| n.as_array()) {
            for n in nodes {
                if let Some(id) = s_str(n, "id") {
                    want.insert(format!("node:{id}"));
                }
            }
        }
        folds.insert(scope.to_string(), fold);
    }
    // Pass 2: ONE verdict sweep, stat-filtered to the lockfiles that exist;
    // the per-key read pays one native verdict each and measured 1.7 s over
    // 122 active rows. The re-fold is pure over entries, so it costs nothing.
    let keys: Vec<String> = want.into_iter().collect();
    // The fold resolves its own claims dir. Every key it asks after is a
    // `node:` key and those route to the global root on both sides, so one
    // resolver answers and no caller has to pass the flag. `--claims-dir`
    // stays an override for tests.
    let resolved = claims_dir
        .cloned()
        .or_else(|| crate::claims::claims_dir_for(None));
    let (workers, sweep_ran) = live_workers(resolved.as_ref(), &keys);
    let mut refolded: BTreeMap<String, Value> = BTreeMap::new();
    for crown in crowns {
        let Some(scope) = s_str(crown, "scope") else {
            continue;
        };
        let level = crown.get("level").and_then(|l| l.as_i64());
        let fold = fold_one(
            scope, level, &entries, &projects, &workers, sweep_ran, now_secs,
        );
        refolded.insert(scope.to_string(), fold);
    }
    let folds = refolded;
    if format == "html-section" {
        let mut html = String::from("<section class=\"court\">");
        html.push_str(section_css());
        html.push_str("<h2>Court</h2>");
        for crown in crowns {
            let Some(scope) = s_str(crown, "scope") else {
                continue;
            };
            let empty = json!({"status": "unresolved", "reason": "the row carried no fold"});
            let fold = folds.get(scope).unwrap_or(&empty);
            html.push_str(&crown_html(crown, fold));
        }
        html.push_str("</section>");
        return Ok(json!({"section": html}));
    }
    let stuck = stuck_verdict(&folds);
    let line = stuck_line(&stuck);
    Ok(json!({"scope_nodes": folds, "stuck": stuck, "stuck_line": line}))
}

/// `fno-agents court-fold`: print the fold JSON or the board section, exit 0.
pub fn run_court_fold(args: &[String]) -> i32 {
    let mut graph: Option<PathBuf> = None;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut claims_dir: Option<PathBuf> = None;
    let mut crowns: Vec<Value> = Vec::new();
    let mut format = "json".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--graph" if i + 1 < args.len() => {
                graph = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--cwd" if i + 1 < args.len() => {
                cwd = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--claims-dir" if i + 1 < args.len() => {
                claims_dir = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--crowns-json" if i + 1 < args.len() => {
                match serde_json::from_str::<Value>(&args[i + 1]) {
                    Ok(Value::Array(list)) => crowns = list,
                    Ok(_) => {
                        eprintln!("fno-agents court-fold: --crowns-json must be a JSON array");
                        return 2;
                    }
                    Err(e) => {
                        eprintln!("fno-agents court-fold: --crowns-json is not JSON: {e}");
                        return 2;
                    }
                }
                i += 2;
            }
            "--format" if i + 1 < args.len() => {
                format = args[i + 1].clone();
                i += 2;
            }
            other => {
                eprintln!("fno-agents court-fold: unknown flag {other}");
                eprintln!(
                    "fno-agents court-fold: --graph PATH [--cwd PATH] [--claims-dir PATH] \
                     --crowns-json JSON [--format json|html-section]"
                );
                return 2;
            }
        }
    }
    let Some(graph) = graph else {
        eprintln!("fno-agents court-fold: --graph is required");
        return 2;
    };
    if format != "json" && format != "html-section" {
        eprintln!("fno-agents court-fold: --format must be json or html-section");
        return 2;
    }
    match court_fold(&graph, &cwd, claims_dir.as_ref(), &crowns, &format) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(e) => {
            eprintln!("fno-agents court-fold: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Value> {
        serde_json::from_str(
            r#"[
            {"id": "e-1", "type": "epic", "status": "in_progress"},
            {"id": "x-1", "parent": "e-1", "status": "in_progress", "pr_number": 3,
             "slug": "x-1-slug",
             "sessions": [{"session_id": "s1"}, {"session_id": "s2"}],
             "session_id": "s3", "cost_sessions": ["s4", "s2"],
             "locked_by_harness_session": "s5"},
            {"id": "x-2", "parent": "e-1", "status": "done"},
            {"id": "x-3", "parent": "e-1", "status": "idea"}
        ]"#,
        )
        .unwrap()
    }

    fn no_projects() -> Result<HashMap<String, String>, String> {
        Ok(HashMap::new())
    }

    #[test]
    fn fold_one_counts_whole_scope_lists_active_states_the_omitted_count() {
        let workers = BTreeMap::new();
        let mut e = entries();
        // Ledger-derived cost sessions arrive as objects, not bare strings;
        // both shapes can coexist in one list.
        e[1]["cost_sessions"] = json!(["s4", {"session_id": "s6", "cost_usd": 0.4}]);
        let fold = fold_one("e-1", Some(2), &e, &no_projects(), &workers, true, 0);
        assert_eq!(fold["status"], "ok");
        assert_eq!(fold["total"], 4);
        // x-2 (done) and x-3 (idea) are the two inactive rows.
        assert_eq!(fold["omitted"], 2);
        let nodes = fold["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["id"], "e-1");
        assert_eq!(nodes[1]["sessions"][0], "s1");
        assert_eq!(nodes[1]["sessions"].as_array().unwrap().len(), 6);
        assert_eq!(nodes[1]["pr_number"], 3);
        assert_eq!(nodes[1]["slug"], "x-1-slug");
        let counts = fold["counts"].as_object().unwrap();
        let sum: i64 = counts.values().map(|v| v.as_i64().unwrap()).sum();
        assert_eq!(sum, 4);
        assert!(counts.contains_key("done") && counts.contains_key("idea"));
    }

    #[test]
    fn fold_one_unresolved_names_the_scope_never_an_empty_table() {
        let workers = BTreeMap::new();
        let fold = fold_one(
            "ghost",
            Some(2),
            &entries(),
            &no_projects(),
            &workers,
            true,
            0,
        );
        assert_eq!(fold["status"], "unresolved");
        assert!(fold["reason"].as_str().unwrap().contains("ghost"));
        assert!(fold.get("nodes").is_none());
    }

    #[test]
    fn fold_one_half_crown_is_unresolved_with_a_reason() {
        let workers = BTreeMap::new();
        let fold = fold_one("alpha", None, &entries(), &no_projects(), &workers, true, 0);
        assert_eq!(fold["status"], "unresolved");
        assert!(fold["reason"].as_str().unwrap().contains("level"));
    }

    #[test]
    fn project_rung_folds_by_canonical_project_without_epic_containment() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), "alpha".to_string());
        let projects = Ok(map);
        let mut entries = entries();
        entries.push(serde_json::json!({"id": "a-1", "project": "a", "status": "ready"}));
        entries.push(serde_json::json!({"id": "a-2", "project": "alpha", "status": "done"}));
        entries.push(serde_json::json!({"id": "b-1", "project": "beta", "status": "ready"}));
        let workers = BTreeMap::new();
        let fold = fold_one("alpha", Some(1), &entries, &projects, &workers, true, 0);
        assert_eq!(fold["status"], "ok");
        assert_eq!(fold["total"], 2);
        assert_eq!(fold["nodes"][0]["id"], "a-1");
    }

    #[test]
    fn crown_html_matches_the_dashboard_contract() {
        let workers = BTreeMap::new();
        let fold = fold_one(
            "e-1",
            Some(2),
            &entries(),
            &no_projects(),
            &workers,
            true,
            0,
        );
        let crown = json!({
            "scope": "e-1", "level": 2, "holder": "king-a", "agree": true,
            "reason": Value::Null
        });
        let html = crown_html(&crown, &fold);
        assert!(html.starts_with("<details class=\"crown\"><summary>"));
        assert!(html.contains("agree yes"));
        assert!(html.contains("class=\"crown-counts\""));
        assert!(html.contains("class=\"crown-nodes\""));

        let bad = json!({
            "scope": "e-1", "level": 2, "holder": "king-b", "agree": false,
            "reason": "'e-1' status is 'done' (terminal)"
        });
        let html = crown_html(&bad, &fold);
        assert!(html.contains("court-disagree"));
        assert!(html.contains("&#x27;"));
    }

    /// One swept row per node id, shaped as `claim_sweep_payload_from_records`
    /// returns them.
    fn swept(rows: &[(&str, &str, &str)]) -> BTreeMap<String, Value> {
        rows.iter()
            .map(|(id, state, holder)| {
                (
                    id.to_string(),
                    json!({
                        "key": format!("node:{id}"),
                        "state": state,
                        "holder": holder,
                        "basis": "live",
                    }),
                )
            })
            .collect()
    }

    #[test]
    fn a_held_node_names_its_holder_and_states_the_claim() {
        let workers = swept(&[("x-1", "live", "worker-a")]);
        let fold = fold_one(
            "e-1",
            Some(2),
            &entries(),
            &no_projects(),
            &workers,
            true,
            0,
        );
        let nodes = fold["nodes"].as_array().unwrap();
        let held = nodes.iter().find(|n| n["id"] == "x-1").unwrap();
        assert_eq!(held["worker"], "worker-a");
        assert_eq!(held["claim_state"], "live");
        assert_eq!(held["claim_basis"], "live");
    }

    #[test]
    fn free_and_no_record_are_two_answers_and_neither_names_a_worker() {
        let workers = swept(&[("x-1", "free", "worker-a")]);
        let fold = fold_one(
            "e-1",
            Some(2),
            &entries(),
            &no_projects(),
            &workers,
            true,
            0,
        );
        let nodes = fold["nodes"].as_array().unwrap();
        let freed = nodes.iter().find(|n| n["id"] == "x-1").unwrap();
        // A holder on an unheld record is history, not an owner.
        assert_eq!(freed["worker"], Value::Null);
        assert_eq!(freed["claim_state"], "free");
        let missing = nodes.iter().find(|n| n["id"] == "e-1").unwrap();
        assert_eq!(missing["claim_state"], "no-record");
        assert_eq!(missing["claim_basis"], Value::Null);
    }

    #[test]
    fn a_sweep_that_did_not_run_reads_unreadable_never_no_record() {
        let fold = fold_one(
            "e-1",
            Some(2),
            &entries(),
            &no_projects(),
            &BTreeMap::new(),
            false,
            0,
        );
        let nodes = fold["nodes"].as_array().unwrap();
        assert!(!nodes.is_empty());
        for n in nodes {
            assert_eq!(n["claim_state"], "unreadable");
            assert_ne!(n["claim_state"], "no-record");
        }
    }

    #[test]
    fn live_workers_reports_a_store_it_never_reached() {
        let missing = PathBuf::from("/nonexistent/fno-court-fold-probe/claims");
        let (rows, ran) = live_workers(Some(&missing), &["node:x-1".to_string()]);
        assert!(rows.is_empty());
        assert!(
            !ran,
            "a directory the scan never read is not an empty store"
        );
        let (rows, ran) = live_workers(None, &["node:x-1".to_string()]);
        assert!(rows.is_empty());
        assert!(!ran);
    }

    #[test]
    fn the_row_carries_age_and_the_blocker_it_waits_on() {
        let mut e = entries();
        e[1]["created_at"] = json!("2026-01-01T00:00:00Z");
        e[1]["blocked_by"] = json!(["x-9"]);
        e[1]["blocked_reason"] = json!("waiting on the contract");
        // 2026-01-01T02:00:00Z, two hours after the stamp above.
        let now = crate::tick_ledger::parse_rfc3339_unix("2026-01-01T02:00:00Z").unwrap();
        let fold = fold_one(
            "e-1",
            Some(2),
            &e,
            &no_projects(),
            &BTreeMap::new(),
            true,
            now,
        );
        let row = fold["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "x-1")
            .unwrap()
            .clone();
        assert_eq!(row["age_hours"], json!(2.0));
        assert_eq!(row["blocked_by"], json!(["x-9"]));
        assert_eq!(row["blocked_reason"], "waiting on the contract");
        // An entry with no stamp answers null, which a reader must not read as new.
        let bare = fold["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "e-1")
            .unwrap()
            .clone();
        assert_eq!(bare["age_hours"], Value::Null);
    }

    /// One crown's fold, shaped as `fold_one` returns it.
    fn folds(rows: &[Value]) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        out.insert(
            "alpha".to_string(),
            json!({"status": "ok", "total": rows.len(), "counts": {},
                   "nodes": rows, "omitted": 0}),
        );
        out
    }

    fn row(id: &str, status: &str, claim: &str, age: f64) -> Value {
        json!({
            "id": id, "slug": "s", "status": status, "worker": Value::Null,
            "claim_state": claim, "claim_basis": Value::Null,
            "pr_number": Value::Null, "sessions": [], "age_hours": age,
            "blocked_by": Value::Null, "blocked_reason": Value::Null,
        })
    }

    #[test]
    fn an_old_unclaimed_ready_node_is_stuck_and_a_held_one_never_is() {
        let v = stuck_verdict(&folds(&[
            row("x-1", "ready", "no-record", 2.0),
            row("x-2", "in_progress", "free", 5.5),
            // Held is not stuck however old it is.
            row("x-3", "ready", "live", 99.0),
            // Young is not stuck however unclaimed it is.
            row("x-4", "ready", "no-record", 0.2),
        ]));
        assert_eq!(v["unclaimed"], json!(["x-1", "x-2"]));
        let line = stuck_line(&v);
        assert!(line.contains("2 ready over 60m with no worker"));
        assert!(!line.contains("x-3") && !line.contains("x-4"));
    }

    #[test]
    fn a_blocked_node_names_what_it_waits_on() {
        let mut r = row("x-1", "blocked", "no-record", 0.1);
        r["blocked_by"] = json!(["x-9", "x-8"]);
        let v = stuck_verdict(&folds(&[r]));
        assert_eq!(v["blocked"][0]["id"], "x-1");
        let line = stuck_line(&v);
        // A count that never says what on is the gap this closes.
        assert!(line.contains("1 blocked"));
        assert!(line.contains("x-8") && line.contains("x-9"));
    }

    #[test]
    fn an_unproven_claim_is_stuck_whatever_its_age() {
        let v = stuck_verdict(&folds(&[
            row("x-1", "ready", "unreadable", 0.1),
            row("x-2", "ready", "corrupted", 0.1),
        ]));
        assert_eq!(v["unproven_claim"], json!(["x-1", "x-2"]));
        assert_eq!(v["unclaimed"], json!([]));
        assert!(stuck_line(&v).contains("2 with an unproven claim"));
    }

    #[test]
    fn a_blind_claim_store_never_erases_what_a_node_is_blocked_on() {
        // Measured 2026-09-12 against the live court: with the claims store
        // unreadable, every row reads `unreadable`, and gating the whole
        // classification on that reported 0 blocked while 12 were blocked in
        // the same graph. `blocked_by` is a graph field and never came from
        // the claim store.
        let mut r = row("x-1", "blocked", "unreadable", 0.1);
        r["blocked_by"] = json!(["x-9"]);
        let v = stuck_verdict(&folds(&[r]));
        assert_eq!(v["unproven_claim"], json!(["x-1"]));
        assert_eq!(v["blocked"][0]["id"], "x-1");
        assert_eq!(v["blocked"][0]["blocked_by"], json!(["x-9"]));
        let line = stuck_line(&v);
        assert!(line.contains("1 blocked"));
        assert!(line.contains("x-9"));
    }

    #[test]
    fn an_unproven_claim_is_never_counted_twice() {
        // The other two buckets read the claim, so an unproven row belongs in
        // `unproven_claim` alone.
        let v = stuck_verdict(&folds(&[
            row("x-1", "ready", "unreadable", 9.0),
            row("x-2", "in_review", "corrupted", 9.0),
        ]));
        assert_eq!(v["unproven_claim"], json!(["x-1", "x-2"]));
        assert_eq!(v["unclaimed"], json!([]));
        assert_eq!(v["in_review"], json!([]));
    }

    #[test]
    fn an_old_review_needs_a_pr_to_count() {
        let mut with_pr = row("x-1", "in_review", "no-record", 4.0);
        with_pr["pr_number"] = json!(12);
        let v = stuck_verdict(&folds(&[
            with_pr,
            row("x-2", "in_review", "no-record", 4.0),
        ]));
        assert_eq!(v["in_review"], json!(["x-1"]));
    }

    #[test]
    fn a_long_clause_names_a_few_and_counts_the_rest() {
        let rows: Vec<Value> = (0..12)
            .map(|i| row(&format!("x-{i:02}"), "ready", "no-record", 4.0))
            .collect();
        let v = stuck_verdict(&folds(&rows));
        // The full list stays in the JSON; only the line is capped.
        assert_eq!(v["unclaimed"].as_array().unwrap().len(), 12);
        let line = stuck_line(&v);
        assert!(line.contains("12 ready over 60m with no worker"));
        assert!(line.contains("x-00, x-01, x-02, x-03, x-04, +7 more"));
        assert!(!line.contains("x-11"));
    }

    #[test]
    fn a_quiet_scope_renders_an_empty_line_never_a_blind_one() {
        let v = stuck_verdict(&folds(&[
            row("x-1", "ready", "live", 99.0),
            row("x-2", "ready", "no-record", 0.2),
        ]));
        assert_eq!(stuck_line(&v), "");
        assert_eq!(v["blind"], json!([]));
    }

    #[test]
    fn a_node_two_crowns_both_cover_is_counted_once() {
        // An L1 crown folds the nodes its L2 epics also fold.
        let node = row("x-1", "ready", "no-record", 3.0);
        let mut two = folds(&[node.clone()]);
        two.insert(
            "beta".to_string(),
            json!({"status": "ok", "total": 1, "counts": {}, "nodes": [node], "omitted": 0}),
        );
        let v = stuck_verdict(&two);
        assert_eq!(v["unclaimed"], json!(["x-1"]));
    }

    #[test]
    fn one_fault_across_every_crown_is_one_line() {
        let mut two = BTreeMap::new();
        for scope in ["alpha", "beta", "gamma"] {
            two.insert(
                scope.to_string(),
                json!({"status": "unresolved", "reason": "the fold timed out after 30s"}),
            );
        }
        let v = stuck_verdict(&two);
        assert_eq!(v["blind"].as_array().unwrap().len(), 1);
        let line = stuck_line(&v);
        assert!(line.contains("could not answer: the fold timed out after 30s"));
        // A blind read must never render as a clean one.
        assert_ne!(line, "");
    }

    #[test]
    fn the_board_section_shows_the_claim_and_the_age() {
        let workers = swept(&[("x-1", "live", "worker-a")]);
        let fold = fold_one(
            "e-1",
            Some(2),
            &entries(),
            &no_projects(),
            &workers,
            true,
            0,
        );
        let crown = json!({
            "scope": "e-1", "level": 2, "holder": "king-a", "agree": true,
            "reason": Value::Null
        });
        let html = crown_html(&crown, &fold);
        assert!(html.contains("<th>claim</th>"));
        assert!(html.contains("<th>age</th>"));
        assert!(html.contains("worker-a"));
        assert!(html.contains("<td>live</td>"));
    }
}
