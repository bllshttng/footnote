//! `fno backlog list <query>`: the node-query door over the shared search
//! grammar (ruling d-069a9fa1: shared CLI behavior lives in this crate,
//! where the grammar and the read model already are). The front door claims
//! the bare group spelling lexically; the saved-set spellings
//! (`list next|ready|...`) stay with the sibling dispatcher's catalog, and
//! everything else in the namespace keeps its handover.

use std::ffi::OsString;
use std::path::Path;

use serde_json::{json, Value};

/// The saved-set actions the sibling catalog maps under the `list` group.
/// A second token naming one of these leaves this door unclaimed.
const SAVED_SET_ACTIONS: &[&str] = &[
    "next",
    "ready",
    "queued",
    "worked",
    "lanes",
    "undispatched",
    "stuck-epics",
];

/// Claim the bare `fno backlog list [<query>] [flags]` spelling. Returns the
/// tail after `list`, or `None` to leave the argv with the sibling
/// dispatcher (a saved-set action, or anything that is not `backlog list`).
pub fn classify(args: &[OsString]) -> Option<Vec<String>> {
    if args.first()?.to_str()? != "backlog" || args.get(1)?.to_str()? != "list" {
        return None;
    }
    let third = args.get(2).and_then(|a| a.to_str());
    if third.is_some_and(|t| SAVED_SET_ACTIONS.contains(&t)) {
        return None;
    }
    Some(
        args[2..]
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
    )
}

const LIST_HELP: &str = "Query the node board in the shared search grammar\n\nUsage: fno backlog list [<query>] [flags]\n\nOne query answers once: the matches, the honest total, no caller-side cutting.\n`fno backlog list <action>` (next|ready|queued|worked|lanes|undispatched|\nstuck-epics) is the saved-set spelling and answers elsewhere.\n\nOptions:\n  --json     JSON: {\"total\": M, \"showing\": N, \"rows\": [...]} (exit 0 when empty)\n  --count    print only the match count (exit 0 when empty)\n  --limit N  show the first N matches; the totals stay honest\n  -h, --help print help\n\nExit codes: 0 matches (or a --json/--count answer), 1 no matches on the\npretty path or an unreadable graph, 2 usage or query parse error.";

/// One output row: the card the board model derives plus a borrow of the
/// row fields the card does not carry.
struct Hit<'a> {
    card: crate::backlog_model::Card,
    row: Option<&'a Value>,
}

impl Hit<'_> {
    fn field(&self, key: &str) -> Option<String> {
        self.row
            .and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn json_row(&self) -> Value {
        json!({
            "id": self.card.id,
            "slug": self.card.slug,
            "title": self.card.title,
            "status": self.card.status,
            "column": self.card.column,
            "priority": self.card.priority,
            "size": self.card.size,
            "difficulty": self.field("difficulty"),
            "type": self.card.kind,
            "project": self.card.project,
            "parent": self.card.parent,
            "pr": self.row.and_then(|r| r.get("pr_number")).cloned().unwrap_or(Value::Null),
            "claimed": self.card.claimed,
            "created_at": self.card.created_at,
            "updated_at": self.card.updated_at,
        })
    }

    fn tsv_row(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.card.id,
            self.card.status.as_deref().unwrap_or("?"),
            self.card.priority.as_deref().unwrap_or("-"),
            self.card.column,
            self.card.parent.as_deref().unwrap_or("-"),
            self.card.title,
        )
    }
}

/// The sort read off `Parsed.sort` (the page key with an optional `-`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortKey {
    Created,
    Updated,
    Votes,
    Priority,
    Id,
    Title,
    Status,
    Size,
    Lead,
}

fn sort_key(page: &str) -> Option<(SortKey, bool)> {
    // The grammar prefixes `-` for descending.
    let (name, desc) = match page.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (page, false),
    };
    let key = match name {
        "created_at" => SortKey::Created,
        "updated_at" => SortKey::Updated,
        "encounters" => SortKey::Votes,
        "priority" => SortKey::Priority,
        "id" => SortKey::Id,
        "title" => SortKey::Title,
        "status" => SortKey::Status,
        "size" => SortKey::Size,
        "lead" => SortKey::Lead,
        _ => return None,
    };
    Some((key, desc))
}

/// One card's sort value. Numeric keys read numbers; unknown/missing ranks
/// last.
fn sort_value(key: SortKey, hit: &Hit) -> SortValue {
    let card = &hit.card;
    match key {
        SortKey::Votes => SortValue::Num(card.encounters as u64),
        SortKey::Priority => SortValue::Num(
            card.priority
                .as_deref()
                .and_then(|p| p.strip_prefix('p'))
                .and_then(|n| n.parse().ok())
                .unwrap_or(u64::MAX),
        ),
        SortKey::Size => SortValue::Num(match card.size.as_deref() {
            Some("s" | "S") => 0,
            Some("m" | "M") => 1,
            Some("l" | "L") => 2,
            _ => u64::MAX,
        }),
        SortKey::Created => SortValue::Str(card.created_at.clone()),
        SortKey::Updated => SortValue::Str(card.updated_at.clone()),
        SortKey::Id => SortValue::Str(Some(card.id.clone())),
        SortKey::Title => SortValue::Str(Some(card.title.to_lowercase())),
        SortKey::Status => SortValue::Str(card.status.clone()),
        SortKey::Lead => SortValue::Str(card.lead.as_ref().map(|l| l.name.to_lowercase())),
    }
}

enum SortValue {
    Str(Option<String>),
    Num(u64),
}

impl SortValue {
    fn cmp(&self, other: &SortValue) -> std::cmp::Ordering {
        match (self, other) {
            (SortValue::Num(a), SortValue::Num(b)) => a.cmp(b),
            (SortValue::Str(a), SortValue::Str(b)) => a.cmp(b),
            // A number never meets a string: keys are single-typed.
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// The run entry: returns the process exit code.
pub fn run(tail: &[String]) -> i32 {
    let mut json_out = false;
    let mut count_only = false;
    let mut limit: Option<usize> = None;
    let mut query_parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        match tail[i].as_str() {
            "--help" | "-h" => {
                println!(
                    "{LIST_HELP}\n\n{}",
                    crate::search_query::help_text(crate::search_query::Surface::Node,)
                );
                return 0;
            }
            "--json" => json_out = true,
            "--count" => count_only = true,
            "--limit" => {
                i += 1;
                match tail.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => limit = Some(n),
                    None => {
                        eprintln!("fno backlog list: --limit needs a number");
                        return 2;
                    }
                }
            }
            other if other.starts_with("--limit=") => match other["--limit=".len()..].parse() {
                Ok(n) => limit = Some(n),
                Err(_) => {
                    eprintln!("fno backlog list: --limit needs a number");
                    return 2;
                }
            },
            // Everything else is query text: the grammar's negation is a
            // leading `-` on the term (`-s:done`, `-blocked`), so only the
            // exact `--` spellings above are flags.
            other if other.starts_with("--") => {
                eprintln!("fno backlog list: unknown flag {other:?} (--help for usage)");
                return 2;
            }
            other => query_parts.push(other),
        }
        i += 1;
    }
    let query = query_parts.join(" ");
    let now = crate::search_query::now_secs();
    let parsed = match crate::search_query::parse(&query, crate::search_query::Surface::Node, now) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno backlog list: {e}");
            return 2;
        }
    };
    let graph = crate::backlog_view::graph_path();
    let mut inputs = match gather_inputs(&graph) {
        Ok(inp) => inp,
        Err(e) => {
            eprintln!("fno backlog list: {e}");
            return 1;
        }
    };
    crate::backlog_model::read_search_sources(&mut inputs, parsed.wants_questions());
    if let Some(err) = &inputs.rows_error {
        eprintln!("fno backlog list: {err}");
        return 1;
    }
    let order = crate::backlog_model::order_of(&inputs);
    let by_ref = crate::backlog_model::board_refs(&inputs);
    let mut child_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in &inputs.rows {
        if let Some(parent) = r
            .get("parent")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        {
            *child_counts.entry(parent).or_insert(0) += 1;
        }
    }
    let mut hits: Vec<Hit> = Vec::new();
    for e in &inputs.rows {
        let blocked = crate::backlog_view::has_open_dependency(e, &by_ref);
        let Some(mut card) = crate::backlog_model::card_of(&inputs, e, order, blocked) else {
            continue;
        };
        card.child_count = child_counts.get(card.id.as_str()).copied().unwrap_or(0);
        card.parent_title = card
            .parent
            .as_deref()
            .and_then(|p| by_ref.get(p))
            .and_then(|r| r.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // An empty query keeps everything: skip the per-row field-map build
        // it would never read.
        if !query.is_empty() {
            let fields = crate::backlog_model::search_fields(
                &inputs,
                &by_ref,
                &card,
                by_ref.get(card.id.as_str()).copied(),
            );
            if !parsed.keeps(&fields) {
                continue;
            }
        }
        let row = by_ref.get(card.id.as_str()).copied();
        hits.push(Hit { card, row });
    }
    let total = hits.len();
    if let Some(sort) = &parsed.sort {
        if let Some((key, desc)) = sort_key(sort) {
            // Board order breaks ties, so one snapshot answers the same
            // order on every run.
            hits.sort_by(|a, b| {
                let ord = sort_value(key, a).cmp(&sort_value(key, b));
                let ord = if desc { ord.reverse() } else { ord };
                ord.then_with(|| a.card.order.cmp(&b.card.order))
            });
        }
    }
    if count_only {
        println!("{total}");
        return 0;
    }
    let shown: Vec<&Hit> = match limit {
        Some(n) => hits.iter().take(n).collect(),
        None => hits.iter().collect(),
    };
    if json_out {
        let rows: Vec<Value> = shown.iter().map(|h| h.json_row()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": total,
                "showing": rows.len(),
                "rows": rows,
            }))
            .unwrap_or_else(|_| "[]".to_string())
        );
        return 0;
    }
    if total == 0 {
        eprintln!("fno backlog list: no nodes match the query");
        return 1;
    }
    for h in &shown {
        println!("{}", h.tsv_row());
    }
    if shown.len() < total {
        println!("showing {} of {total}", shown.len());
    }
    0
}

/// The gather the snapshot door runs: a current-thread runtime over the
/// model's async gather, the crowned roster read once beside it.
fn gather_inputs(graph: &Path) -> Result<crate::backlog_model::Inputs, String> {
    let now = crate::search_query::now_secs();
    let agents = std::fs::read_to_string(crate::agents_view::registry_path())
        .ok()
        .and_then(|raw| crate::agents_view::derive_rows(&raw, now as u64))
        .map(|rows: Vec<crate::agents_view::RegistryAgent>| {
            rows.iter()
                .filter(|r| !r.exited && r.crown_scope.is_some())
                .map(|r| crate::proto::AgentRow {
                    name: r.name.clone(),
                    crown_level: r.crown_level,
                    crown_scope: r.crown_scope.clone(),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();
    let graph = graph.to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    Ok(runtime.block_on(crate::backlog_model::gather(&graph, agents)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn the_bare_spelling_claims_and_the_saved_set_does_not() {
        assert_eq!(
            classify(&os(&["backlog", "list"])).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            classify(&os(&["backlog", "list", "--json"])).unwrap(),
            vec!["--json".to_string()]
        );
        assert_eq!(
            classify(&os(&["backlog", "list", "s:ready", "p:high"])).unwrap(),
            vec!["s:ready".to_string(), "p:high".to_string()]
        );
        for action in SAVED_SET_ACTIONS {
            assert!(classify(&os(&["backlog", "list", action])).is_none());
        }
        assert!(classify(&os(&["backlog", "get", "x-1"])).is_none());
        assert!(classify(&os(&["agents", "list"])).is_none());
    }

    #[test]
    fn sort_keys_read_the_page_names_and_the_desc_flag() {
        assert_eq!(sort_key("priority"), Some((SortKey::Priority, false)));
        assert_eq!(sort_key("-created_at"), Some((SortKey::Created, true)));
        assert_eq!(sort_key("nonsense"), None);
    }

    #[test]
    fn priority_and_size_rank_missing_last() {
        let mk = |priority: Option<&str>, size: Option<&str>| Hit {
            card: crate::backlog_model::Card {
                priority: priority.map(str::to_string),
                size: size.map(str::to_string),
                ..clone_card()
            },
            row: None,
        };
        let p0 = mk(Some("p0"), None);
        let p1 = mk(Some("p1"), None);
        let none = mk(None, None);
        assert_eq!(
            sort_value(SortKey::Priority, &p0).cmp(&sort_value(SortKey::Priority, &p1)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            sort_value(SortKey::Priority, &p1).cmp(&sort_value(SortKey::Priority, &none)),
            std::cmp::Ordering::Less
        );
        let big = mk(None, Some("L"));
        let small = mk(None, Some("S"));
        assert_eq!(
            sort_value(SortKey::Size, &small).cmp(&sort_value(SortKey::Size, &big)),
            std::cmp::Ordering::Less
        );
    }

    fn clone_card() -> crate::backlog_model::Card {
        crate::backlog_model::Card {
            id: "x-1".to_string(),
            slug: None,
            title: "t".to_string(),
            column: "Ready",
            order: 0,
            rank: None,
            priority: None,
            size: None,
            status: None,
            project: None,
            parent: None,
            kind: None,
            tags: vec![],
            blocked: false,
            claimed: false,
            lead: None,
            live: false,
            created_at: None,
            completed_at: None,
            updated_at: None,
            child_count: 0,
            parent_title: None,
            session_ids: vec![],
            encounters: 0,
            encounters_operator: 0,
        }
    }
}
