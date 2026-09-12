//! `reign-ledger`: the reign ledger page for `fno agents king ledger`.
//!
//! Python resolves the court (registry adjudication, manifest limbs, the
//! caller's paths) and hands one court JSON over; the native side owns the
//! page assembly, the same split `king-history` applies to the journal
//! readback, so the Python-tree ratchet holds. The crown-to-nodes join stays
//! in the fold the Python side already ran (`scope_nodes` rides in the court
//! JSON); titles, omitted members, uncrowned epics, and orphan leaves are
//! read from the graph through the SAME compiler `court-fold` uses, so the
//! page cannot disagree with the court about who holds a node.
//!
//! `reign-ledger --court-json PATH --graph PATH --generated TS --out PATH`
//!
//! rc 0 wrote the page, 1 render or read failure, 2 usage failure.
use crate::court_fold::{compile_forced, esc, ACTIVE_STATUSES, COUNT_ORDER};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn as_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn plural(n: i64, one: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        format!("{one}s")
    }
}

/// The counts fragment in lifecycle order; a status outside the vocabulary
/// keeps its place at the end rather than vanishing.
fn counts_line(fold: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(counts) = fold.get("counts").and_then(|c| c.as_object()) {
        for status in COUNT_ORDER {
            if let Some(n) = counts.get(status).and_then(|v| v.as_i64()) {
                parts.push(format!("{status} {n}"));
            }
        }
        let mut leftovers: Vec<String> = counts
            .keys()
            .filter(|k| !COUNT_ORDER.contains(&k.as_str()))
            .cloned()
            .collect();
        leftovers.sort();
        for key in leftovers {
            let n = counts.get(&key).and_then(|v| v.as_i64()).unwrap_or(0);
            parts.push(format!("{key} {n}"));
        }
    }
    parts.join(", ")
}

fn titles_of(entries: &[Value]) -> BTreeMap<String, &Value> {
    let mut out = BTreeMap::new();
    for e in entries {
        if let Some(id) = s_str(e, "id") {
            out.insert(id.to_string(), e);
        }
    }
    out
}

fn row_tr(n: &Value, titles: &BTreeMap<String, &Value>) -> String {
    let id = s_str(n, "id").unwrap_or("");
    let title = titles
        .get(id)
        .and_then(|e| s_str(e, "title"))
        .unwrap_or("-");
    let worker = n
        .get("worker")
        .map(|w| match w {
            Value::Null => String::new(),
            Value::String(s) => esc(s),
            other => esc(&other.to_string()),
        })
        .unwrap_or_default();
    let pr = n
        .get("pr_number")
        .and_then(|p| p.as_i64())
        .map(|p| format!("#{p}"))
        .unwrap_or_default();
    format!(
        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
        esc(id),
        esc(title),
        esc(s_str(n, "status").unwrap_or("")),
        worker,
        pr
    )
}

const TABLE_HEAD: &str = "<table><thead><tr><th>node</th><th>title</th><th>status</th><th>worker</th><th>pr</th></tr></thead><tbody>";

/// The member ids of one crown at its own level, or None when the scope
/// cannot compile (the fold's verdict already says so in place).
fn members_of(
    crown: &Value,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> Option<BTreeSet<String>> {
    let scope = s_str(crown, "scope")?;
    let level = crown.get("level").and_then(|l| l.as_i64())?;
    compile_forced(scope, entries, projects, level).ok()
}

fn crown_section(
    crown: &Value,
    titles: &BTreeMap<String, &Value>,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> String {
    let level = match crown.get("level").and_then(|l| l.as_i64()) {
        Some(l) => format!("L{l}"),
        None => "L?".to_string(),
    };
    let agree = match crown.get("agree") {
        Some(Value::Bool(true)) => "yes",
        Some(Value::Bool(false)) => "no",
        _ => "?",
    };
    let mut out = format!(
        "<section class=\"crown\"><h2>{} &middot; {} &middot; {}</h2><p class=\"meta\">grantor {} &middot; status {} &middot; agree {agree}</p>",
        esc(s_str(crown, "scope").unwrap_or("-")),
        level,
        esc(s_str(crown, "holder").unwrap_or("-")),
        esc(s_str(crown, "grantor").unwrap_or("-")),
        esc(s_str(crown, "status").unwrap_or("-")),
    );
    if let Some(reason) = s_str(crown, "reason") {
        out.push_str(&format!("<p class=\"meta\">{}</p>", esc(reason)));
    }
    let fold = crown.get("scope_nodes").cloned().unwrap_or(json!({}));
    if s_str(&fold, "status") == Some("unresolved") {
        out.push_str(&format!(
            "<p class=\"note\">scope fold: unresolved - {}</p>",
            esc(s_str(&fold, "reason").unwrap_or(""))
        ));
    } else {
        out.push_str(&format!(
            "<p class=\"counts\">{} nodes: {} ({} not listed)</p>",
            as_i64(&fold, "total"),
            esc(&counts_line(&fold)),
            as_i64(&fold, "omitted"),
        ));
        let listed = fold
            .get("nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        let mut rows: Vec<String> = listed.iter().map(|n| row_tr(n, titles)).collect();
        // The omitted half of the scope renders with titles too: a count with
        // no names is a number where a fact should be.
        if let Some(members) = members_of(crown, entries, projects) {
            let listed_ids: BTreeSet<&str> = listed.iter().filter_map(|n| s_str(n, "id")).collect();
            for id in &members {
                if listed_ids.contains(id.as_str()) {
                    continue;
                }
                let entry = titles.get(id).copied().cloned().unwrap_or(json!({}));
                rows.push(format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td></td><td></td></tr>",
                    esc(id),
                    esc(s_str(&entry, "title").unwrap_or("-")),
                    esc(s_str(&entry, "status").unwrap_or("")),
                ));
            }
        }
        out.push_str(&format!("{TABLE_HEAD}{}</tbody></table>", rows.join("")));
    }
    out.push_str("</section>");
    out
}

/// Epics in no crown's territory: absent from every fold, so the page names
/// them instead of letting their absence read as zero.
fn uncrowned_section(
    crowns: &[Value],
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> String {
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for crown in crowns {
        if let Some(members) = members_of(crown, entries, projects) {
            covered.extend(members);
        }
    }
    let mut orphans: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            s_str(e, "type") == Some("epic") && !covered.contains(s_str(e, "id").unwrap_or(""))
        })
        .collect();
    if orphans.is_empty() {
        return String::new();
    }
    let p1 = orphans
        .iter()
        .filter(|e| s_str(e, "priority") == Some("p1"))
        .count();
    orphans.sort_by_key(|e| {
        (
            s_str(e, "priority").unwrap_or("p2").to_string(),
            s_str(e, "title").unwrap_or("").to_string(),
        )
    });
    let rows: String = orphans
        .iter()
        .map(|e| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(s_str(e, "id").unwrap_or("")),
                esc(s_str(e, "title").unwrap_or("-")),
                esc(s_str(e, "status").unwrap_or("")),
                esc(s_str(e, "priority").unwrap_or("")),
            )
        })
        .collect();
    format!(
        "<section class=\"crown\"><h2>uncrowned epics</h2><p class=\"meta\">{} uncrowned, {p1} at p1</p>\
         <table><thead><tr><th>node</th><th>title</th><th>status</th><th>priority</th></tr></thead><tbody>{rows}</tbody></table></section>",
        orphans.len()
    )
}

/// Orphan leaves: parent falsy, status actionable, and the id is no node's
/// parent - work nobody contains and nobody contains the container of.
fn orphan_leaves_section(entries: &[Value]) -> String {
    let parents: BTreeSet<&str> = entries.iter().filter_map(|e| s_str(e, "parent")).collect();
    let mut leaves: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            s_str(e, "parent").is_none()
                && ACTIVE_STATUSES.contains(&s_str(e, "status").unwrap_or(""))
                && !parents.contains(s_str(e, "id").unwrap_or(""))
        })
        .collect();
    if leaves.is_empty() {
        return String::new();
    }
    let p1 = leaves
        .iter()
        .filter(|e| s_str(e, "priority") == Some("p1"))
        .count();
    leaves.sort_by_key(|e| {
        (
            s_str(e, "priority").unwrap_or("p2").to_string(),
            s_str(e, "title").unwrap_or("").to_string(),
        )
    });
    let rows: String = leaves
        .iter()
        .map(|e| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(s_str(e, "id").unwrap_or("")),
                esc(s_str(e, "title").unwrap_or("-")),
                esc(s_str(e, "status").unwrap_or("")),
                esc(s_str(e, "priority").unwrap_or("")),
            )
        })
        .collect();
    format!(
        "<section class=\"crown\"><h2>orphan leaves</h2><p class=\"meta\">{} {}, {p1} at p1</p>\
         <table><thead><tr><th>node</th><th>title</th><th>status</th><th>priority</th></tr></thead><tbody>{rows}</tbody></table></section>",
        leaves.len(),
        plural(leaves.len() as i64, "orphan leaf"),
    )
}

const CSS: &str = "body{font-family:-apple-system,'Segoe UI',sans-serif;margin:24px auto;max-width:900px;color:#1a1a2e}\
h1{font-size:20px;margin:0 0 4px}.meta{color:#6b7280;font-size:12px;margin:2px 0}\
.note{color:#b91c1c;font-size:12.5px}\
section.crown{border:1px solid #e5e7eb;border-radius:8px;padding:10px 14px;margin:14px 0}\
section.crown h2{font-size:14px;margin:0 0 4px;font-family:ui-monospace,monospace}\
table{width:100%;border-collapse:collapse;font-size:12px;font-family:ui-monospace,monospace}\
th{text-align:left;color:#6b7280;font-weight:500;padding:3px 8px 3px 0;border-bottom:1px solid #e5e7eb}\
td{padding:3px 8px 3px 0;border-bottom:1px solid #f3f4f6;word-break:break-all}";

/// The whole page. An empty court renders "no live crowns"; an unreadable
/// registry renders the named reason. Never a blank or falsely healthy page.
pub fn render(court: &Value, entries: &[Value], generated: &str) -> String {
    let projects: Result<HashMap<String, String>, String> =
        crate::king_board::project_map(&std::env::current_dir().unwrap_or_default());
    let summary = court.get("summary").cloned().unwrap_or(json!({}));
    let crowns = court.get("crowns").and_then(|c| c.as_array()).cloned();
    let titles = titles_of(entries);
    let mut out = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Reign Ledger</title><style>{CSS}</style></head><body>\
         <h1>Reign Ledger</h1><p class=\"meta\">generated {generated}</p>"
    );
    match &crowns {
        None => out.push_str(&format!(
            "<p class=\"note\">court: CANNOT READ - {}. This is not an empty court; nothing was checked.</p>",
            esc(s_str(&summary, "reason").unwrap_or("reason unavailable"))
        )),
        Some(crowns) if crowns.is_empty() => {
            out.push_str("<p class=\"meta\">no live crowns</p>");
        }
        Some(_) => {
            let total = as_i64(&summary, "total");
            let disagreements = as_i64(&summary, "disagreements");
            let unknowns = as_i64(&summary, "unknowns");
            let splits = as_i64(&summary, "splits");
            out.push_str(&format!(
                "<p class=\"meta\">{} {}, {} {}, {} {}, {} {}</p>",
                total,
                plural(total, "crown"),
                disagreements,
                plural(disagreements, "disagreement"),
                unknowns,
                plural(unknowns, "unknown"),
                splits,
                plural(splits, "split"),
            ));
            let manifest_only = as_i64(&summary, "manifest_only");
            if manifest_only > 0 {
                out.push_str(&format!(
                    "<p class=\"meta\">{} manifest-only</p>",
                    manifest_only
                ));
            }
        }
    }
    if crowns.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
        && summary.get("sweep_ran") == Some(&Value::Bool(false))
    {
        out.push_str(
            "<p class=\"note\">orphan sweep did not run (stale or missing binary): \
             zero manifest-only entries is an absence, not a finding</p>",
        );
    }
    if let Some(crowns) = &crowns {
        for crown in crowns {
            out.push_str(&crown_section(crown, &titles, entries, &projects));
        }
        if !entries.is_empty() {
            out.push_str(&uncrowned_section(crowns, entries, &projects));
            out.push_str(&orphan_leaves_section(entries));
        }
    }
    out.push_str("</body></html>");
    out
}

fn write_atomic(path: &PathBuf, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot publish {}: {e}", path.display()))?;
    Ok(())
}

pub fn run_reign_ledger(args: &[String]) -> i32 {
    let mut court_json: Option<PathBuf> = None;
    let mut graph: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut generated = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--court-json" if i + 1 < args.len() => {
                court_json = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--graph" if i + 1 < args.len() => {
                graph = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--generated" if i + 1 < args.len() => {
                generated = args[i + 1].clone();
                i += 2;
            }
            "--out" if i + 1 < args.len() => {
                out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => {
                eprintln!("fno-agents reign-ledger: unknown flag {other}");
                eprintln!(
                    "fno-agents reign-ledger: --court-json PATH --graph PATH \
                     --generated TS --out PATH"
                );
                return 2;
            }
        }
    }
    let (Some(court_path), Some(graph_path), Some(out_path)) = (court_json, graph, out) else {
        eprintln!("fno-agents reign-ledger: --court-json, --graph and --out are required");
        return 2;
    };
    let court_text = match std::fs::read_to_string(&court_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "fno-agents reign-ledger: cannot read {}: {e}",
                court_path.display()
            );
            return 1;
        }
    };
    let court: Value = match serde_json::from_str(&court_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fno-agents reign-ledger: court JSON is not JSON: {e}");
            return 1;
        }
    };
    let entries: Vec<Value> =
        match crate::graph_store::read_defaulted_opts(&graph_path, false, false) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("fno-agents reign-ledger: graph unreadable: {e}");
                return 1;
            }
        };
    if let Err(e) = write_atomic(&out_path, &render(&court, &entries, &generated)) {
        eprintln!("fno-agents reign-ledger: {e}");
        return 1;
    }
    println!("reign ledger: {}", out_path.display());
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(court: Value, entries: Vec<Value>) -> String {
        render(&court, &entries, "2026-09-12T00:00:00Z")
    }

    fn base_crown() -> Value {
        json!({
            "holder": "king", "level": 2, "scope": "e-1", "grantor": "human",
            "status": "busy", "agree": true, "reason": null, "crown_source": "row",
            "scope_nodes": {"status": "ok", "counts": {"in_progress": 1, "done": 2},
                "total": 3, "omitted": 1,
                "nodes": [{"id": "x-1", "status": "in_progress", "worker": "w1",
                           "pr_number": 7, "sessions": ["s1"]}]}
        })
    }

    fn base_court(crowns: Value) -> Value {
        json!({"crowns": crowns, "conflicts": [], "registry_readable": true,
               "graph_readable": true,
               "summary": {"total": 1, "manifest_only": 0, "sweep_ran": true,
                           "disagreements": 0, "unknowns": 0, "splits": 0}})
    }

    #[test]
    fn one_section_per_crown_names_scope_and_holder() {
        let court = base_court(json!([base_crown(), base_crown()]));
        let page = page(court, vec![]);
        assert_eq!(page.matches("<section").count(), 2);
        assert!(page.contains("e-1") && page.contains("king"));
    }

    #[test]
    fn unresolved_fold_states_its_reason_in_place() {
        let mut crown = base_crown();
        crown["scope_nodes"] = json!({"status": "unresolved", "reason": "boom"});
        let page = page(base_court(json!([crown])), vec![]);
        assert!(page.contains("scope fold: unresolved - boom"));
        assert!(!page.contains("<table"));
    }

    #[test]
    fn empty_court_renders_the_measurement() {
        let page = page(base_court(json!([])), vec![]);
        assert!(page.contains("no live crowns"));
    }

    #[test]
    fn registry_unreadable_names_the_reason() {
        let court = json!({"crowns": null, "registry_readable": false,
            "summary": {"reason": "registry unreadable: disk on fire"}});
        let page = page(court, vec![]);
        assert!(page.contains("registry unreadable: disk on fire"));
        assert!(!page.contains("no live crowns"));
    }

    #[test]
    fn hostile_fields_are_escaped() {
        let mut crown = base_crown();
        crown["holder"] = json!("<script>x</script>");
        crown["scope_nodes"]["nodes"] = json!([{"id": "x-1", "status": "in_progress",
            "worker": "<img>", "pr_number": 7}]);
        let page = page(base_court(json!([crown])), vec![]);
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<img"));
    }

    #[test]
    fn counts_render_in_lifecycle_order() {
        let mut crown = base_crown();
        crown["scope_nodes"]["counts"] = json!({"zebra": 1, "done": 2, "in_progress": 1});
        let page = page(base_court(json!([crown])), vec![]);
        assert!(page.contains("in_progress 1, done 2, zebra 1"));
    }

    #[test]
    fn omitted_members_render_with_titles() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "title": "the crown epic", "status": "in_progress"}),
            json!({"id": "x-done", "parent": "e-1", "title": "shipped thing", "status": "done"}),
        ];
        let page = page(base_court(json!([base_crown()])), entries);
        assert!(page.contains("shipped thing"));
        assert!(page.contains("the crown epic"));
    }

    #[test]
    fn uncrowned_epics_get_their_own_section() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "title": "reigned epic", "status": "ready", "priority": "p2"}),
            json!({"id": "x-9", "parent": "e-1", "title": "contained", "status": "in_progress"}),
            json!({"id": "e-2", "type": "epic", "title": "free one", "status": "ready", "priority": "p2"}),
            json!({"id": "e-3", "type": "epic", "title": "urgent orphan", "status": "idea", "priority": "p1"}),
        ];
        let whole = page(base_court(json!([base_crown()])), entries);
        let at = whole.find("uncrowned epics").expect("uncrowned section");
        let section = &whole[at..];
        assert!(section.contains("2 uncrowned, 1 at p1"));
        assert!(section.contains("free one") && section.contains("urgent orphan"));
        assert!(!section.contains("reigned epic"));
    }

    #[test]
    fn orphan_leaves_follow_the_structural_rule() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "parent": null, "status": "in_progress"}),
            json!({"id": "x-1", "parent": "e-1", "status": "in_progress", "title": "contained", "priority": "p1"}),
            json!({"id": "l-1", "title": "free leaf", "status": "ready", "priority": "p1"}),
            json!({"id": "l-2", "title": "done leaf", "status": "done", "priority": "p1"}),
            json!({"id": "l-3", "title": "container", "status": "in_progress", "priority": "p2"}),
            json!({"id": "l-4", "parent": "l-3", "status": "ready"}),
        ];
        let whole = page(base_court(json!([base_crown()])), entries);
        let at = whole.find("orphan leaves").expect("leaves section");
        let section = &whole[at..];
        assert!(section.contains("1 orphan leaf, 1 at p1"));
        assert!(section.contains("free leaf"));
        assert!(!section.contains("done leaf"));
        assert!(!section.contains("container"));
        assert!(!section.contains("contained"));
    }

    #[test]
    fn writes_atomically_and_names_the_path() {
        let dir = std::env::temp_dir().join(format!("reign-ledger-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let court_path = dir.join("court.json");
        let graph_path = dir.join("graph.json");
        let out_path = dir.join("reign.html");
        std::fs::write(&court_path, base_court(json!([base_crown()])).to_string()).unwrap();
        std::fs::write(&graph_path, json!({"entries": []}).to_string()).unwrap();
        let args: Vec<String> = [
            "--court-json",
            court_path.to_str().unwrap(),
            "--graph",
            graph_path.to_str().unwrap(),
            "--generated",
            "2026-09-12T00:00:00Z",
            "--out",
            out_path.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_reign_ledger(&args), 0);
        assert!(out_path.exists());
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftovers);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
