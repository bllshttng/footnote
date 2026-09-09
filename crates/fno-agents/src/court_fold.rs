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
const ACTIVE_STATUSES: [&str; 5] = ["in_progress", "in_review", "ready", "blocked", "design"];

/// Counts render in lifecycle order; a status outside the vocabulary keeps
/// its place at the end rather than vanishing from the line.
const COUNT_ORDER: [&str; 9] = [
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

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// html.escape(quote=True) semantics: the section lands in a document the
/// scripted board also writes into, and the same bytes must escape the same
/// way on both sides.
fn esc(s: &str) -> String {
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
fn compile_forced(
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

/// Node id -> holder for live/suspect claims over the asked keys, through the
/// same native verdict machinery `claim sweep` uses. Any fault degrades to an
/// empty map: a display read never raises.
fn live_workers(claims_dir: Option<&PathBuf>, keys: &[String]) -> BTreeMap<String, String> {
    let Some(dir) = claims_dir else {
        return BTreeMap::new();
    };
    let records = crate::claims::list_in(std::slice::from_ref(dir), None, true);
    let payload = crate::claim_verbs::claim_sweep_payload_from_records(&records, None, keys, false);
    let mut out = BTreeMap::new();
    if let Some(rows) = payload.get("claims").and_then(|v| v.as_array()) {
        for row in rows {
            let key = s_str(row, "key").unwrap_or_default().to_string();
            if row.get("state").and_then(|s| s.as_str()) != Some("live")
                && row.get("state").and_then(|s| s.as_str()) != Some("suspect")
            {
                continue;
            }
            if let Some(holder) = row.get("holder").and_then(|h| h.as_str()) {
                out.insert(
                    key.trim_start_matches("node:").to_string(),
                    holder.to_string(),
                );
            }
        }
    }
    out
}

/// The fold for one crown: counts over the whole scope, rows for the active
/// statuses only, and `omitted` stated, never implied.
fn fold_one(
    scope: &str,
    level: Option<i64>,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
    workers: &BTreeMap<String, String>,
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
        nodes.push(json!({
            "id": id,
            "slug": s_str(entry, "slug").unwrap_or(""),
            "status": status,
            "worker": workers.get(id).cloned().map(Value::String).unwrap_or(Value::Null),
            "pr_number": entry.get("pr_number").cloned().unwrap_or(Value::Null),
            "sessions": sessions,
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
         <th>pr</th><th>sessions</th></tr></thead><tbody>",
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
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(s_str(r, "id").unwrap_or("")),
                esc(s_str(r, "status").unwrap_or("")),
                esc(s_str(r, "worker").unwrap_or("-")),
                esc(&pr),
                esc(&sessions),
            ));
        }
    }
    out.push_str("</tbody></table></div></details>");
    out
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
    // Pass 1: fold with no workers named, collecting the node ids the worker
    // sweep will ask after.
    let mut want: BTreeSet<String> = BTreeSet::new();
    let mut folds: BTreeMap<String, Value> = BTreeMap::new();
    for crown in crowns {
        let Some(scope) = s_str(crown, "scope") else {
            continue;
        };
        let level = crown.get("level").and_then(|l| l.as_i64());
        let fold = fold_one(scope, level, &entries, &projects, &BTreeMap::new());
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
    let workers = live_workers(claims_dir, &keys);
    let mut refolded: BTreeMap<String, Value> = BTreeMap::new();
    for crown in crowns {
        let Some(scope) = s_str(crown, "scope") else {
            continue;
        };
        let level = crown.get("level").and_then(|l| l.as_i64());
        let fold = fold_one(scope, level, &entries, &projects, &workers);
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
    Ok(json!({"scope_nodes": folds}))
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
        // Ledger-derived cost sessions arrive as objects, not bare strings.
        e[2]["cost_sessions"] = json!([{"session_id": "s6", "cost_usd": 0.4}]);
        let fold = fold_one("e-1", Some(2), &e, &no_projects(), &workers);
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
        let fold = fold_one("ghost", Some(2), &entries(), &no_projects(), &workers);
        assert_eq!(fold["status"], "unresolved");
        assert!(fold["reason"].as_str().unwrap().contains("ghost"));
        assert!(fold.get("nodes").is_none());
    }

    #[test]
    fn fold_one_half_crown_is_unresolved_with_a_reason() {
        let workers = BTreeMap::new();
        let fold = fold_one("alpha", None, &entries(), &no_projects(), &workers);
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
        let fold = fold_one("alpha", Some(1), &entries, &projects, &workers);
        assert_eq!(fold["status"], "ok");
        assert_eq!(fold["total"], 2);
        assert_eq!(fold["nodes"][0]["id"], "a-1");
    }

    #[test]
    fn crown_html_matches_the_dashboard_contract() {
        let workers = BTreeMap::new();
        let fold = fold_one("e-1", Some(2), &entries(), &no_projects(), &workers);
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
}
