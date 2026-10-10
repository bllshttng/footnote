//! Native triage CLI: the `triage` grouped actions over the engine in
//! `super::triage`. Wired actions: context, propose, rank, validate,
//! projects. The apply mutation (locked store write), the consistency
//! runner (headless LLM one-shots), and the health/trend metrics fold
//! their remaining Python contract ranges before the door routes them;
//! until then the grouped door keeps forwarding those names to the wheel.

use serde_json::{json, Value};

use super::triage;

fn echo_json(value: &Value) {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    println!("{text}");
}

fn usage() -> i32 {
    eprintln!("usage: fno backlog triage <context|propose|rank|validate|projects> [args]");
    2
}

/// The grouped door's entry: dispatch on the action token; an unknown or
/// missing action prints the usage line and exits 2.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("context") => run_context(&args[1..]),
        Some("propose") => run_propose(&args[1..]),
        Some("rank") => run_rank(&args[1..]),
        Some("validate") => run_validate(&args[1..]),
        Some("projects") => run_projects(&args[1..]),
        _ => usage(),
    }
}

/// The guarded store read the diagnostic surfaces share: an external
/// selection or an unreadable graph refuses with exit 2, never stale rows.
fn triage_entries_or_exit() -> Result<Vec<Value>, i32> {
    triage::triage_entries().map_err(|e| {
        eprintln!("fno triage: {e}");
        2
    })
}

/// Filter to a roadmap and a project scope the way the collectors do.
fn scoped(entries: Vec<Value>, roadmap_id: Option<&str>) -> Vec<Value> {
    match roadmap_id {
        Some(id) => entries
            .into_iter()
            .filter(|e| e.get("roadmap_id").and_then(Value::as_str) == Some(id))
            .collect(),
        None => entries,
    }
}

/// The pending and idea collectors, sorted by priority then created_at.
fn collect(entries: &[Value], deep: bool, idea: bool) -> Vec<Value> {
    let mut picked: Vec<Value> = entries
        .iter()
        .filter(|e| {
            if idea {
                triage::is_idea(e)
            } else {
                triage::is_pending(e)
            }
        })
        .cloned()
        .collect();
    triage::sort_entries_by_priority_created(&mut picked);
    picked
        .iter()
        .map(|e| triage::candidate_record(e, deep))
        .collect()
}

fn parse_flags(args: &[String]) -> (bool, Option<String>, Option<String>, Option<String>, bool) {
    // (deep, project, roadmap_id, positional, json)
    let mut deep = false;
    let mut project: Option<String> = None;
    let mut roadmap_id: Option<String> = None;
    let mut positional: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--deep" => deep = true,
            "--project" => project = it.next().cloned(),
            "--roadmap-id" => roadmap_id = it.next().cloned(),
            a if a.starts_with('-') => {}
            a => positional = Some(a.to_string()),
        }
    }
    (deep, project, roadmap_id, positional, false)
}

/// `triage context`: the LLM-reasoning context payload.
pub fn run_context(args: &[String]) -> i32 {
    let (deep, _project, roadmap_id, _pos, _json) = parse_flags(args);
    let entries = match triage_entries_or_exit() {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    let rows = scoped(entries, roadmap_id.as_deref());
    echo_json(&json!({
        "candidates": collect(&rows, deep, false),
        "ideas": collect(&rows, deep, true),
        "inbox_items": [],
        "goals": [],
    }));
    0
}

/// `triage propose`: the proposal skeleton (heuristic-only here; the LLM
/// fill is the caller's step).
pub fn run_propose(args: &[String]) -> i32 {
    let (deep, _project, roadmap_id, _pos, _json) = parse_flags(args);
    let entries = match triage_entries_or_exit() {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    let rows = scoped(entries, roadmap_id.as_deref());
    let candidates = collect(&rows, deep, false);
    let ideas = collect(&rows, deep, true);
    let proposal = json!({
        "dependencies": [],
        "priority_changes": [],
        "duplicates": [],
        "defer": [],
        "candidates": candidates,
        "ideas": ideas,
        "scope": "all projects",
    });
    echo_json(&proposal);
    0
}

/// `triage rank`: fold pairwise verdicts into one total order (Copeland).
/// Participants are verdict ids that exist in the graph; unknown ids drop,
/// mirroring validate. Reads a `--verdicts FILE` or stdin.
pub fn run_rank(args: &[String]) -> i32 {
    let mut verdicts_file: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--verdicts" {
            verdicts_file = it.next().cloned();
        }
    }
    let raw = match &verdicts_file {
        Some(path) => std::fs::read_to_string(path),
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).map(|_| buf)
        }
    };
    let raw = match raw {
        Ok(text) => text,
        Err(e) => {
            eprintln!("Error: verdicts unreadable ({e})");
            return 2;
        }
    };
    let data: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: verdicts are not valid JSON: {e}");
            return 2;
        }
    };
    let empty: Vec<Value> = Vec::new();
    let pairs = match &data {
        Value::Array(rows) => rows.clone(),
        Value::Object(obj) => obj
            .get("verdicts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => empty.clone(),
    };
    if !data.is_array() && !data.is_object() {
        eprintln!("Error: verdicts must be a JSON list of {{winner, loser}}");
        return 2;
    }
    let entries = match triage_entries_or_exit() {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    let by_id: std::collections::BTreeMap<String, &Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e))
        })
        .collect();
    // Participants are verdict ids that actually exist in the graph.
    let mut ids: Vec<String> = Vec::new();
    for v in &pairs {
        if let Some(obj) = v.as_object() {
            for key in ["winner", "loser"] {
                if let Some(val) = obj.get(key).and_then(Value::as_str) {
                    if by_id.contains_key(val) {
                        ids.push(val.to_string());
                    }
                }
            }
        }
    }
    let mut meta: std::collections::BTreeMap<String, (i64, String)> =
        std::collections::BTreeMap::new();
    for id in &ids {
        let entry = by_id.get(id).copied();
        meta.insert(
            id.clone(),
            (
                triage::priority_order(
                    entry.and_then(|e| e.get("priority").and_then(Value::as_str)),
                ),
                entry
                    .and_then(|e| e.get("created_at").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string(),
            ),
        );
    }
    let mut ranked = triage::copeland_rank(&ids, &pairs, Some(&meta));
    for r in ranked.iter_mut() {
        if let Some(obj) = r.as_object_mut() {
            let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
            let title = by_id
                .get(id)
                .and_then(|e| e.get("title"))
                .cloned()
                .unwrap_or(Value::Null);
            obj.insert("title".to_string(), title);
        }
    }
    echo_json(&json!({"order": ranked}));
    0
}

/// `triage validate`: drop cycles and unknown-id entries, print cleaned
/// JSON, exit 3 when any entry was dropped.
pub fn run_validate(args: &[String]) -> i32 {
    let (_deep, _project, _roadmap, positional, _json) = parse_flags(args);
    let Some(path) = positional else {
        eprintln!("Error: validate needs the path to proposal.json");
        return 2;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("Error: no proposal file at {path}");
            return 2;
        }
        Err(e) => {
            eprintln!("Error: proposal is not readable ({path}): {e}");
            return 2;
        }
    };
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: proposal at {path} is not valid JSON: {e}");
            return 2;
        }
    };
    let entries = match triage_entries_or_exit() {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    let (mut cleaned, errors) = triage::validate_proposal(&data, &entries);
    for err in &errors {
        eprintln!("{err}");
    }
    if let Some(obj) = cleaned.as_object_mut() {
        obj.insert("validation_errors".to_string(), json!(errors));
    }
    echo_json(&cleaned);
    if cleaned
        .get("validation_errors")
        .and_then(Value::as_array)
        .is_some_and(|rows| !rows.is_empty())
    {
        return 3;
    }
    0
}

/// `triage projects`: pending counts per project, alphabetical.
pub fn run_projects(args: &[String]) -> i32 {
    let mut roadmap_id: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--roadmap-id" {
            roadmap_id = it.next().cloned();
        }
    }
    let entries = match triage_entries_or_exit() {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for e in &entries {
        if roadmap_id
            .as_deref()
            .is_some_and(|id| e.get("roadmap_id").and_then(Value::as_str) != Some(id))
        {
            continue;
        }
        if !triage::is_pending(e) {
            continue;
        }
        // Legacy entries without a project field would produce an
        // unroutable triage pass; skip rather than grouping under a
        // sentinel bucket.
        if let Some(proj) = e.get("project").and_then(Value::as_str) {
            *counts.entry(proj.to_string()).or_insert(0) += 1;
        }
    }
    let out: Vec<Value> = counts
        .into_iter()
        .map(|(name, n)| json!({"name": name, "pending_count": n}))
        .collect();
    echo_json(&json!({"projects": out}));
    0
}
