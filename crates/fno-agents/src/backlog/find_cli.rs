//! `fno backlog find`'s search ladder, ported from the Python command the
//! grouped dispatcher routes here (the Python leg is deleted). Owns: the
//! exact tiers, the ab- prefix tier (resolve_id's id branch), the
//! describe-it token-subset search over title+slug+details, the four
//! filters, the archive read-through on a miss, and the two render shapes
//! (pretty JSON, TSV handle lines). `--fts` rides the documented substring
//! degrade, and an external backend selection refuses with the guarded
//! metadata reader's exact message; everything else is byte-faithful.

use serde_json::Value;

/// The parsed find invocation. `None` is the usage-error shape (an unknown
/// flag, a second positional, or no query).
struct FindArgs<'a> {
    query: &'a str,
    domain: Option<&'a str>,
    project: Option<&'a str>,
    status: Option<&'a str>,
    source_kind: Option<&'a str>,
    fts: bool,
    json_out: bool,
}

impl FindArgs<'_> {
    fn parse<'a>(tail: &'a [String]) -> Option<FindArgs<'a>> {
        let mut query: Option<&'a str> = None;
        let mut domain: Option<&'a str> = None;
        let mut project: Option<&'a str> = None;
        let mut status: Option<&'a str> = None;
        let mut source_kind: Option<&'a str> = None;
        let mut fts = false;
        let mut json_out = false;
        let mut i = 0;
        while i < tail.len() {
            match tail[i].as_str() {
                "--domain" | "-d" => {
                    i += 1;
                    domain = Some(tail.get(i)?.as_str());
                }
                "--project" | "-p" => {
                    i += 1;
                    project = Some(tail.get(i)?.as_str());
                }
                "--status" | "-s" => {
                    i += 1;
                    status = Some(tail.get(i)?.as_str());
                }
                "--source-kind" => {
                    i += 1;
                    source_kind = Some(tail.get(i)?.as_str());
                }
                // Accepted for compatibility; the native find has no FTS
                // cache, so the flag rides the documented substring degrade.
                "--fts" => fts = true,
                "--limit" | "-L" => {
                    i += 1;
                    tail.get(i)?;
                }
                "--json" | "-J" => json_out = true,
                other => {
                    if other.starts_with('-') {
                        return None;
                    }
                    if query.is_some() {
                        return None;
                    }
                    query = Some(other);
                }
            }
            i += 1;
        }
        query.map(|query| FindArgs {
            query,
            domain,
            project,
            status,
            source_kind,
            fts,
            json_out,
        })
    }
}

const FIND_HELP: &str = "Search graph entries: exact id/slug/bare-hex, else high-recall over title+slug+details\n\nUsage: fno backlog find [OPTIONS] <QUERY>\n\nArguments:\n  <QUERY>  ab-id / id-prefix / slug / bare-hex / free-text description\n\nOptions:\n  -d, --domain <DOMAIN>     Filter by domain\n  -p, --project <PROJECT>   Filter by project\n  -s, --status <STATUS>     Filter by status\n      --source-kind <KIND>  Filter by origin: organic|from_inbox|from_observation|from_supervisor|operator_request\n      --fts                 Full-text search; the native find degrades to substring search\n  -L, --limit <LIMIT>       Max results for the --fts lane [default: 20]\n  -J, --json                Emit JSON array\n  -h, --help                Print help";

/// The describe-it search: every query token must appear in the
/// concatenated title+slug+details text, case-insensitive, non-done first.
fn search_entries(entries: &[Value], query: &str) -> Vec<Value> {
    let tokens: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let hit = |e: &Value| {
        let mut text = String::new();
        for field in ["title", "slug", "details"] {
            if let Some(s) = e.get(field).and_then(Value::as_str) {
                text.push_str(&s.to_lowercase());
                text.push(' ');
            }
        }
        tokens.iter().all(|t| text.contains(t.as_str()))
    };
    let (non_done, done): (Vec<&Value>, Vec<&Value>) = entries
        .iter()
        .partition(|e| e.get("status").and_then(Value::as_str) != Some("done"));
    non_done
        .into_iter()
        .chain(done)
        .filter(|e| hit(e))
        .cloned()
        .collect()
}

/// The id-prefix tier for an `ab-` query: 4-7 hex suffix matches ids by
/// prefix; a full 8-hex or malformed suffix returns nothing (resolve_id's
/// id branch, byte-faithful).
fn ab_prefix_hits(entries: &[Value], query: &str) -> Vec<Value> {
    let suffix = &query["ab-".len()..];
    let full = query.len() == 3 + 8 && suffix.bytes().all(|b| b.is_ascii_hexdigit());
    let partial = (4..=7).contains(&suffix.len()) && suffix.bytes().all(|b| b.is_ascii_hexdigit());
    if full {
        return Vec::new();
    }
    if partial {
        return entries
            .iter()
            .filter(|e| {
                e.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.starts_with(query))
            })
            .cloned()
            .collect();
    }
    Vec::new()
}

/// The exact tiers (the same three get uses), returning the candidates.
fn exact_tier(entries: &[Value], query: &str) -> Option<Vec<Value>> {
    for e in entries {
        if e.get("id").and_then(Value::as_str) == Some(query) {
            return Some(vec![e.clone()]);
        }
    }
    let q_lc = query.to_lowercase();
    for e in entries {
        let slug = e.get("slug").and_then(Value::as_str);
        if slug.is_some_and(|s| !s.is_empty()) && slug == Some(q_lc.as_str()) {
            return Some(vec![e.clone()]);
        }
    }
    let bare_hex = query.len() >= 4
        && query.len() <= 8
        && query
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if bare_hex {
        let mut prefixes = vec![super::settings::node_id_prefix()];
        prefixes.push("ab-".to_string());
        prefixes.dedup();
        for p in &prefixes {
            let cand = format!("{p}{query}");
            if let Some(hit) = entries
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(cand.as_str()))
            {
                return Some(vec![hit.clone()]);
            }
        }
    }
    None
}

/// The four filters, reading the resolved source_kind default.
fn passes_filters(e: &Value, args: &FindArgs<'_>) -> bool {
    if let Some(d) = args.domain {
        if e.get("domain").and_then(Value::as_str) != Some(d) {
            return false;
        }
    }
    if let Some(p) = args.project {
        if e.get("project").and_then(Value::as_str) != Some(p) {
            return false;
        }
    }
    if let Some(s) = args.status {
        if e.get("status").and_then(Value::as_str) != Some(s) {
            return false;
        }
    }
    if let Some(sk) = args.source_kind {
        let resolved = e
            .get("source_kind")
            .and_then(Value::as_str)
            .unwrap_or("organic");
        if resolved != sk {
            return false;
        }
    }
    true
}

/// The display handle: `slug (id)`, or `(id)` when unslugged.
fn format_handle(e: &Value) -> String {
    let nid = e.get("id").and_then(Value::as_str).unwrap_or("?");
    match e.get("slug").and_then(Value::as_str) {
        Some(slug) if !slug.is_empty() => format!("{slug} ({nid})"),
        _ => format!("({nid})"),
    }
}

/// The whole ladder. `run` returns the process exit code.
pub fn run(tail: &[String]) -> i32 {
    if tail.iter().any(|t| t == "--help" || t == "-h") {
        println!("{FIND_HELP}");
        return 0;
    }
    let Some(args) = FindArgs::parse(tail) else {
        eprintln!(
            "fno backlog find: usage: fno backlog find <query> [-d DOMAIN] [-p PROJECT] \
             [-s STATUS] [--source-kind KIND] [--fts] [-L LIMIT] [-J] (--help for detail)"
        );
        return 2;
    };
    // The Python surface refused here with the guarded metadata reader's
    // message (an external tracker has its own UI; a stale local render is
    // the leak the seam closes). Same bytes, same exit.
    if crate::graph_get::external_backend_selected() {
        eprintln!(
            "fno backlog: find: footnote-minted node metadata (size/type/project/\
             dep/model pins/persistent slug) lives only in the default graph \
             store and is unavailable under an external tracker backend"
        );
        return 2;
    }
    if args.fts {
        eprintln!(
            "warning: fts unavailable (the native find has no FTS cache); \
             using substring search"
        );
    }
    let graph_path = super::settings::graph_path();
    // The store's own rows, verbatim the way the keeper serves them; the
    // `--json` render re-orders into the model dump shape, the TSV rows read
    // the live fields off the raw row.
    let mut entries = match crate::backlog::read_entries(&graph_path) {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!(
                "Could not read the graph cleanly, so '{}' cannot be searched: {}",
                args.query, err
            );
            return super::get_cli::GRAPH_UNREADABLE_EXIT;
        }
    };
    // The search pool carries the defaults tail and the read-time derived
    // status and blocked reason, the same overlay the keeper's served rows
    // answer with. The live pool excludes archived residents; they surface
    // only through the read-through on a live miss (the `archived_at`
    // partition python's include-archived read answered with).
    crate::graph_store::apply_defaults(&mut entries, false);
    crate::graph_store::apply_readiness_overlay(&mut entries);
    let live: Vec<Value> = entries
        .iter()
        .filter(|r| r.get("archived_at").is_none())
        .cloned()
        .collect();
    let mut matched = resolve_against(&live, &args);
    matched.retain(|e| passes_filters(e, &args));
    if matched.is_empty() {
        let archived: Vec<Value> = entries
            .iter()
            .filter(|r| r.get("archived_at").is_some())
            .cloned()
            .collect();
        let mut hits = resolve_against(&archived, &args);
        hits.retain(|e| passes_filters(e, &args));
        for h in hits {
            let mut row = h.clone();
            if let Some(obj) = row.as_object_mut() {
                obj.insert("_archived".to_string(), Value::Bool(true));
            }
            matched.push(row);
        }
    }
    if matched.is_empty() {
        eprintln!("fno backlog find: no matches for {:?}", args.query);
        return 1;
    }
    if args.json_out {
        let ordered: Vec<Value> = matched.iter().map(model_order).collect();
        println!("{}", super::render::py_json_pretty(&Value::Array(ordered)));
        return 0;
    }
    for e in &matched {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            format_handle(e),
            e.get("status").and_then(Value::as_str).unwrap_or("?"),
            e.get("domain").and_then(Value::as_str).unwrap_or("?"),
            e.get("project").and_then(Value::as_str).unwrap_or("-"),
            e.get("title").and_then(Value::as_str).unwrap_or(""),
        );
    }
    0
}

/// Resolve against one pool: exact tiers, then the ab- prefix tier, then the
/// describe-it search.
fn resolve_against(pool: &[Value], args: &FindArgs<'_>) -> Vec<Value> {
    if let Some(candidates) = exact_tier(pool, args.query) {
        return candidates;
    }
    if args.query.starts_with("ab-") {
        return ab_prefix_hits(pool, args.query);
    }
    search_entries(pool, args.query)
}

/// The JSON row's display order: the typed model dump order the Python
/// surface served, captured from its output before deletion. Keys outside
/// the list keep their original order at the end.
const MODEL_KEY_ORDER: &[&str] = &[
    "id",
    "parent",
    "children",
    "slug",
    "title",
    "type",
    "project",
    "cwd",
    "priority",
    "rank",
    "domain",
    "blocked_by",
    "related",
    "locked_by",
    "locked_by_harness",
    "locked_by_harness_session",
    "session_id",
    "locked_at",
    "ownership_defect",
    "completed_at",
    "deferred_at",
    "deferred_reason",
    "deferred_kind",
    "touched_at",
    "has_brief",
    "roadmap_id",
    "vision_path",
    "details",
    "cost_usd",
    "cost_sessions",
    "contained_in",
    "size",
    "model",
    "batch",
    "dispatch_verb",
    "dispatch_brief",
    "plan_path",
    "company_work",
    "pr_number",
    "pr_url",
    "additional_prs",
    "merge_status",
    "caused_by",
    "fixes_pr",
    "reverted",
    "artifact_url",
    "completion_note",
    "created_at",
    "superseded_by",
    "supersession",
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "spawned_by_session",
    "spawned_by_harness",
    "spawned_by_cwd",
    "request_origin",
    "origin_evidence",
    "sessions",
    "persisted_status",
    "archived_at",
    "progress_notes",
    "tags",
    "collisions_acknowledged",
    "supersedes",
    "source_kind",
    "source_project",
    "source_session_id",
    "source_inbox_msg",
    "decisions",
    "queued_at",
    "queued_reason",
    "blocked_reason",
    "status",
];

/// The model's Optional fields, null in the dump when the row lacks them.
/// Pinned from the captured output; a schema addition that gains a null
/// default extends this list in the same commit as MODEL_KEY_ORDER.
const MODEL_NULL_DEFAULTS: &[&str] = &[
    "parent",
    "children",
    "rank",
    "locked_by",
    "locked_by_harness",
    "locked_by_harness_session",
    "session_id",
    "locked_at",
    "ownership_defect",
    "completed_at",
    "deferred_at",
    "deferred_reason",
    "deferred_kind",
    "has_brief",
    "roadmap_id",
    "vision_path",
    "details",
    "cost_usd",
    "cost_sessions",
    "contained_in",
    "size",
    "model",
    "batch",
    "dispatch_verb",
    "dispatch_brief",
    "plan_path",
    "company_work",
    "pr_number",
    "pr_url",
    "merge_status",
    "caused_by",
    "fixes_pr",
    "reverted",
    "artifact_url",
    "completion_note",
    "superseded_by",
    "supersession",
    "source_harness",
    "source_cwd",
    "source_node_id",
    "source_plan_path",
    "spawned_by_session",
    "spawned_by_harness",
    "spawned_by_cwd",
    "request_origin",
    "origin_evidence",
    "source_project",
    "source_session_id",
    "source_inbox_msg",
    "queued_at",
    "queued_reason",
    "persisted_status",
    "archived_at",
    "blocked_reason",
    "status",
];

/// The model's typed non-Option defaults: a bool that reads false, the list
/// fields that read empty, and the source_kind vocabulary default.
const MODEL_FALSE_DEFAULTS: &[&str] = &["reverted", "has_brief"];
const MODEL_ORGANIC_DEFAULTS: &[&str] = &["source_kind"];
const MODEL_EMPTY_ARRAY_DEFAULTS: &[&str] = &[
    "children",
    "blocked_by",
    "related",
    "collisions_acknowledged",
    "supersedes",
    "decisions",
    "sessions",
    "progress_notes",
    "cost_sessions",
    "tags",
    "additional_prs",
];

/// Reorder one row's keys into the model dump order, extras appended, the
/// model's typed defaults present: null for the Optionals, false for the
/// bool, empty lists for the list fields, and persisted_status mirrors the
/// row's stored status word.
fn model_order(row: &Value) -> Value {
    let Some(obj) = row.as_object() else {
        return row.clone();
    };
    let mut out = serde_json::Map::new();
    for key in MODEL_KEY_ORDER {
        if let Some(v) = obj.get(*key) {
            out.insert((*key).to_string(), v.clone());
        } else if *key == "persisted_status" {
            out.insert(
                (*key).to_string(),
                obj.get("status").cloned().unwrap_or(Value::Null),
            );
        } else if MODEL_FALSE_DEFAULTS.contains(key) {
            out.insert((*key).to_string(), Value::Bool(false));
        } else if MODEL_ORGANIC_DEFAULTS.contains(key) {
            out.insert((*key).to_string(), Value::String("organic".to_string()));
        } else if MODEL_EMPTY_ARRAY_DEFAULTS.contains(key) {
            out.insert((*key).to_string(), Value::Array(Vec::new()));
        } else if MODEL_NULL_DEFAULTS.contains(key) {
            out.insert((*key).to_string(), Value::Null);
        }
    }
    for (k, v) in obj {
        if !MODEL_KEY_ORDER.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_order_leads_with_id_parent_children_and_appends_extras() {
        let row = json!({"extra": 1, "status": "ready", "id": "x-1", "parent": null});
        let out = model_order(&row);
        let keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // id and parent lead, the typed children default follows at its model
        // position, status sits where the model carries it, and the unknown
        // key lands last.
        assert_eq!(&keys[..3], &["id", "parent", "children"]);
        assert!(keys.contains(&"status"), "{keys:?}");
        assert_eq!(keys.last(), Some(&"extra"));
    }

    #[test]
    fn the_search_orders_non_done_first_and_matches_every_token() {
        let pool = vec![
            json!({"id": "x-1", "title": "beta done node", "status": "done"}),
            json!({"id": "x-2", "title": "alpha live node", "status": "ready"}),
        ];
        let hits = search_entries(&pool, "node");
        assert_eq!(hits[0]["id"], json!("x-2"));
        assert_eq!(hits.len(), 2);
        assert!(!search_entries(&pool, "alpha live").is_empty());
        let no = search_entries(&pool, "");
        assert!(no.is_empty());
    }

    #[test]
    fn the_ab_prefix_tier_matches_partial_hex_never_full_or_malformed() {
        let pool = vec![json!({"id": "ab-bbbb2222", "title": "Beta"})];
        assert_eq!(ab_prefix_hits(&pool, "ab-bbbb").len(), 1);
        assert!(ab_prefix_hits(&pool, "ab-bbbb2222").is_empty());
        assert!(ab_prefix_hits(&pool, "ab-nothex").is_empty());
    }

    #[test]
    fn the_handle_leads_with_the_slug() {
        assert_eq!(format_handle(&json!({"id": "x-1", "slug": "s"})), "s (x-1)");
        assert_eq!(format_handle(&json!({"id": "x-1"})), "(x-1)");
    }

    #[test]
    fn filters_read_the_source_kind_default() {
        let args = FindArgs {
            query: "q",
            domain: None,
            project: None,
            status: None,
            source_kind: Some("operator_request"),
            fts: false,
            json_out: false,
        };
        let organic = json!({"id": "x-1"});
        assert!(!passes_filters(&organic, &args));
        let explicit = json!({"id": "x-1", "source_kind": "operator_request"});
        assert!(passes_filters(&explicit, &args));
    }
}
