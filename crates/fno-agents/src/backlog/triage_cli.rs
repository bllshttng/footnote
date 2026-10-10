//! Native triage CLI: all nine grouped actions over the engine in
//! `super::triage` and the report folds in `super::triage_health`:
//! context, propose, consistency, rank, validate, apply, projects,
//! health, trend. The graph mutation (apply) rides the store's locked
//! write; the consistency runs ride the bounded LLM one-shot seam.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::triage;
use super::triage_health::{run_health, run_trend};

pub(crate) fn echo_json(value: &Value) {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    println!("{text}");
}

const USAGE: &str =
    "usage: fno backlog triage <context|propose|consistency|rank|validate|apply|projects|health|trend> [args]";

fn usage() -> i32 {
    eprintln!("{USAGE}");
    2
}

/// The grouped door's entry: dispatch on the action token; help prints the
/// usage line on stdout and exits 0, an unknown or missing action prints it
/// on stderr and exits 2.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            0
        }
        Some("context") => run_context(&args[1..]),
        Some("propose") => run_propose(&args[1..]),
        Some("rank") => run_rank(&args[1..]),
        Some("validate") => run_validate(&args[1..]),
        Some("projects") => run_projects(&args[1..]),
        Some("consistency") => run_consistency(&args[1..]),
        Some("apply") => run_apply(&args[1..]),
        Some("health") => run_health(&args[1..]),
        Some("trend") => run_trend(&args[1..]),
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

fn parse_flags(
    args: &[String],
) -> (
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    bool,
) {
    let mut deep = false;
    let mut project: Option<String> = None;
    let mut roadmap_id: Option<String> = None;
    let mut positional: Option<String> = None;
    let mut all_projects = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--deep" => deep = true,
            "--all" | "-A" => all_projects = true,
            "--project" => project = it.next().cloned(),
            "--roadmap-id" => roadmap_id = it.next().cloned(),
            a if a.starts_with('-') => {}
            a => positional = Some(a.to_string()),
        }
    }
    (deep, project, roadmap_id, positional, false, all_projects)
}
/// `triage context`: the LLM-reasoning context payload.
pub fn run_context(args: &[String]) -> i32 {
    let (deep, project, roadmap_id, _pos, _json, all_projects) = parse_flags(args);
    match triage::build_context(
        deep,
        all_projects,
        project.as_deref(),
        roadmap_id.as_deref(),
    ) {
        Ok(context) => {
            echo_json(&context);
            0
        }
        Err(e) => {
            eprintln!("fno triage: {e}");
            2
        }
    }
}

/// `triage propose`: the proposal skeleton, or a dry-run candidate summary.
pub fn run_propose(args: &[String]) -> i32 {
    let (deep, project, roadmap_id, _pos, _json, all_projects) = parse_flags(args);
    let dry_run = args.iter().any(|a| a == "--dry-run" || a == "-N");
    let entries = match triage_entries_or_exit() {
        Ok(rows) => rows,
        Err(code) => return code,
    };
    let scope = triage::resolve_scope(project.as_deref(), all_projects, &entries);
    let scoped = triage::filter_by_project(&entries, project.as_deref(), all_projects);
    let candidates = triage::collect_candidates(&scoped, roadmap_id.as_deref(), deep, false);
    let ideas = triage::collect_candidates(&scoped, roadmap_id.as_deref(), deep, true);
    let proposal = json!({
        "dependencies": [],
        "priority_changes": [],
        "duplicates": [],
        "defer": [],
        "candidates": candidates,
        "ideas": ideas,
        "scope": scope,
    });
    if candidates.is_empty() {
        eprintln!("no pending nodes to triage (scope: {scope})");
        echo_json(&proposal);
        return 0;
    }
    if dry_run {
        eprintln!(
            "Proposed triage for {} pending nodes",
            proposal["candidates"].as_array().map(Vec::len).unwrap_or(0)
        );
        eprintln!("Scope: {scope}");
        eprintln!("(dry-run: no LLM call, showing candidates only)");
        eprintln!();
        if let Some(rows) = proposal["candidates"].as_array() {
            for c in rows {
                eprintln!(
                    "  {} [{}] {}",
                    c["id"].as_str().unwrap_or(""),
                    c["priority"].as_str().unwrap_or(""),
                    c["title"].as_str().unwrap_or("")
                );
            }
        }
    }
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
    let (_deep, _project, _roadmap, positional, _json, _all) = parse_flags(args);
    let Some(path) = positional else {
        eprintln!("Error: validate needs the path to proposal.json");
        return 2;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("Error: proposal file not found: {path}");
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

// ---------------------------------------------------------------------------
// The consistency engine: the headless propose runs, the fold, the CLI.
// ---------------------------------------------------------------------------

/// The reasoning instruction handed to each headless run. Mirrors the
/// /triage skill's reasoning prompt so the consistency measurement reflects
/// what production /triage does; when one changes, change both.
const CONSISTENCY_PROMPT: &str = concat!(
    "You are a backlog triage classifier. First REASON, then LABEL - never emit ",
    "the JSON first. In a short reasoning pass, name each spec's PRIMARY concern ",
    "(when a spec raises several concerns, classify on the primary, not the ",
    "loudest surface signal). Then output an optimal ordering as JSON with four ",
    "keys: `dependencies` (edges {from,to,reason} where `to` is blocked_by ",
    "`from`), `priority_changes` ({id,to,reason} where `to` is one of ",
    "p0/p1/p2/p3), `defer` ({id,reason}), and `duplicates` ({ids:[...],reason}). ",
    "Every entry MUST include a one-line `reason`. Do not propose self-edges or ",
    "cycles. Only reason over the `candidates` array; never propose changes for ",
    "`ideas`.",
);

const CONSISTENCY_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "dependencies": {"type": "array", "items": {"type": "object"}},
    "priority_changes": {"type": "array", "items": {"type": "object"}},
    "defer": {"type": "array", "items": {"type": "object"}},
    "duplicates": {"type": "array", "items": {"type": "object"}}
  },
  "required": ["priority_changes"]
}"#;

/// One bounded, tool-less Claude call through the shared seam (fno/llm.py
/// llm_call): FNO_LLM_STUB replaces the binary under tests, a real call is
/// `claude -p --output-format json` with the prompt on stdin, schema and
/// system prompt ride their flags, and any failure names its cause.
fn llm_one_shot(
    prompt: &str,
    schema: Option<&str>,
    system_prompt: Option<&str>,
    model: Option<&str>,
    timeout_s: u64,
) -> Result<String, String> {
    let stub = std::env::var("FNO_LLM_STUB").unwrap_or_default();
    let stub = stub.trim().to_string();
    let cmd = if stub.is_empty() {
        let mut c = std::process::Command::new("claude");
        c.arg("-p");
        c.args(["--output-format", "json"]);
        if let Some(schema) = schema {
            c.args(["--json-schema", schema]);
        }
        if let Some(system_prompt) = system_prompt {
            c.args(["--append-system-prompt", system_prompt]);
        }
        if let Some(model) = model {
            c.args(["--model", model]);
        }
        c
    } else {
        std::process::Command::new(&stub)
    };
    let out = crate::bounded_cmd::output_with_timeout_stdin(cmd, timeout_s, prompt)
        .map_err(|e| format!("claude -p failed to run: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err_text = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "claude -p exited {}: {}",
            out.status.code().unwrap_or(-1),
            err_text.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(stdout)
}

/// ONE headless propose over the frozen context (triage.py
/// _run_consistency_propose): run the model, unwrap the envelope
/// (structured_output, or the `result` text re-parsed, or a direct stub
/// proposal identified by its priority_changes key), and require the
/// priority_changes key so an underfilled envelope is an errored run.
fn consistency_run_propose(context: &Value, model: Option<&str>) -> Result<Value, String> {
    let prompt = format!(
        "{}\n\nCONTEXT:\n{}",
        CONSISTENCY_PROMPT,
        serde_json::to_string(context).map_err(|e| e.to_string())?
    );
    let stdout = llm_one_shot(
        &prompt,
        Some(CONSISTENCY_SCHEMA),
        Some("You are a triage agent. Respond with JSON only."),
        model,
        300,
    )?;
    let data: Value =
        serde_json::from_str(&stdout).map_err(|e| format!("model output is not JSON: {e}"))?;
    let mut proposal = data;
    if proposal.get("priority_changes").is_none() {
        if proposal.get("is_error").and_then(Value::as_bool) == Some(true) {
            let detail = proposal
                .get("result")
                .or_else(|| proposal.get("error"))
                .map(value_brief)
                .unwrap_or_default();
            return Err(format!("claude -p error: {detail}"));
        }
        let structured = proposal
            .get("structured_output")
            .cloned()
            .unwrap_or(Value::Null);
        if structured.is_object() {
            proposal = structured;
        } else {
            let result_text = proposal.get("result").cloned().unwrap_or(Value::Null);
            if let Some(text) = result_text.as_str() {
                let parsed: Value = serde_json::from_str(text)
                    .map_err(|e| format!("claude -p result is not JSON: {e}"))?;
                proposal = parsed;
            }
        }
    }
    if !proposal.is_object() {
        return Err("proposal is not a JSON object".to_string());
    }
    if proposal.get("priority_changes").is_none() {
        return Err("proposal missing required priority_changes".to_string());
    }
    Ok(proposal)
}

/// One-line digest of a JSON value for an error string.
fn value_brief(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `triage consistency`: K headless propose runs over ONE frozen context,
/// folded into per-category agreement. Read-only toward the live graph.
pub fn run_consistency(args: &[String]) -> i32 {
    let mut repeat: i64 = 3;
    let mut frozen: Option<String> = None;
    let mut yes = false;
    let mut model: Option<String> = None;
    let mut deep = false;
    let mut all_projects = false;
    let mut project: Option<String> = None;
    let mut roadmap_id: Option<String> = None;
    let mut json_output = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--repeat" | "-k" => {
                repeat = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            "--frozen-context" => frozen = it.next().cloned(),
            "--yes" => yes = true,
            "--model" => model = it.next().cloned(),
            "--deep" => deep = true,
            "--all" | "-A" => all_projects = true,
            "--project" => project = it.next().cloned(),
            "--roadmap-id" => roadmap_id = it.next().cloned(),
            "--json" | "-J" => json_output = true,
            _ => {}
        }
    }
    if repeat < 1 {
        eprintln!("--repeat must be >= 1");
        return 2;
    }
    let context = match &frozen {
        Some(path) => {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: frozen context unreadable ({path}): {e}");
                    return 2;
                }
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Error: frozen context is not valid JSON ({path}): {e}");
                    return 2;
                }
            }
        }
        None => match triage::build_context(
            deep,
            all_projects,
            project.as_deref(),
            roadmap_id.as_deref(),
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fno triage: {e}");
                return 2;
            }
        },
    };
    let empty = Value::Array(vec![]);
    let candidates = context.get("candidates").unwrap_or(&empty);
    if candidates.as_array().map(Vec::is_empty).unwrap_or(true) {
        eprintln!("nothing to propose (no candidates in the frozen context)");
        return 0;
    }
    if repeat > 10 && !yes {
        eprintln!("--repeat {repeat} makes {repeat} real LLM calls; pass --yes to confirm.");
        return 2;
    }
    let repeat_u = repeat as usize;
    let mut proposals: Vec<Value> = Vec::new();
    let mut errored = 0i64;
    for i in 0..repeat_u {
        match consistency_run_propose(&context, model.as_deref()) {
            Ok(p) => proposals.push(p),
            Err(e) => {
                errored += 1;
                eprintln!("run {}/{repeat} errored: {e}", i + 1);
            }
        }
    }
    let agreement = if proposals.is_empty() {
        json!({})
    } else {
        triage::fold_consistency(&proposals)
    };
    let report = json!({
        "repeat": repeat,
        "completed": proposals.len(),
        "errored": errored,
        "agreement": agreement,
    });
    if json_output {
        echo_json(&report);
        return 0;
    }
    println!(
        "Triage consistency: {}/{repeat} runs completed ({errored} errored)",
        proposals.len()
    );
    if repeat == 1 {
        eprintln!("  note: K=1 measures nothing (a single run trivially agrees with itself)");
    }
    if proposals.is_empty() {
        eprintln!("  no completed runs; agreement not computed");
        return 0;
    }
    if let Some(cats) = agreement.as_object() {
        for (cat, ag) in cats {
            let total = ag["total"].as_i64().unwrap_or(0);
            if total == 0 {
                continue;
            }
            println!(
                "  {cat}: {}/{total} agree",
                ag["agree"].as_i64().unwrap_or(0)
            );
            let dis = ag["disagreeing"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if !dis {
                let names: Vec<String> = ag["disagreeing"]
                    .as_array()
                    .map(|a| a.iter().map(value_brief).collect())
                    .unwrap_or_default();
                println!("    disagreeing: {}", names.join(", "));
            }
        }
    }
    0
}

/// A proposal file's load with the CLI's own error lines (triage.py
/// _load_proposal): missing and malformed are exit-2 refusals.
fn load_proposal_or_exit(path: &str) -> Result<Value, i32> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("Error: proposal file not found: {path}");
            Err(2)
        }
        Err(e) => {
            eprintln!("Error: proposal is not readable ({path}): {e}");
            Err(2)
        }
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!("Error: proposal at {path} is not valid JSON: {e}");
                Err(2)
            }
        },
    }
}

/// `triage apply`: apply a validated proposal under one locked mutation
/// (triage.py cmd_apply). The proposal revalidates against the snapshot
/// that actually publishes (a Conflict retry re-runs the whole fold), so a
/// racing writer can never sneak a cycle in; a partial apply still emits
/// its telemetry and exits 3.
pub fn run_apply(args: &[String]) -> i32 {
    let mut proposal_path: Option<String> = None;
    let mut pick_raw: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--pick" => pick_raw = it.next().cloned(),
            a if a.starts_with('-') => {}
            a => proposal_path = Some(a.to_string()),
        }
    }
    let Some(path) = proposal_path else {
        eprintln!("Error: apply needs the path to proposal.json");
        return 2;
    };
    let data = match load_proposal_or_exit(&path) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let pick_ids: Option<BTreeSet<String>> = pick_raw.map(|raw| {
        raw.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    });
    let graph = super::settings::graph_path();
    let mut locked_errors: Vec<String>;
    let mut applied = json!({
        "dependencies": 0,
        "priority_changes": 0,
        "duplicates_flagged": 0,
        "deferred": 0,
    });
    let mut priority_moves: Vec<Value> = Vec::new();
    let mut attempt = 0;
    let outcome = loop {
        attempt += 1;
        let entries = match crate::backlog::read_entries(&graph) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("Error: graph unreadable: {e}");
                return 2;
            }
        };
        let mut working = entries.clone();
        crate::graph_store::apply_defaults(&mut working, false);
        let (cleaned, errors) = triage::validate_proposal(&data, &working);
        let cleaned = filter_pick(cleaned, pick_ids.as_ref());
        locked_errors = errors.clone();
        mutate_locked_entries(&cleaned, &mut working, &mut applied, &mut priority_moves);
        let input = crate::graph_store::MutateInput {
            entries: working,
            canonical_path: None,
            base_version: match crate::graph_store::base_version(&graph) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Error: graph version unreadable: {e}");
                    return 2;
                }
            },
            plan_rungs: None,
        };
        match crate::graph_store::locked_mutate(&graph, input, std::time::Duration::from_secs(30)) {
            Ok(outcome) => break outcome,
            Err(crate::graph_store::StoreError::Conflict) if attempt < 5 => {}
            Err(e) => {
                eprintln!("Error: apply could not commit: {e}");
                return 2;
            }
        }
    };
    let _ = outcome;
    let proposed: i64 = ["dependencies", "priority_changes", "duplicates", "defer"]
        .iter()
        .map(|k| {
            data.get(*k)
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0) as i64
        })
        .sum();
    let dropped = locked_errors.len() as i64;
    let applied_v = applied.clone();
    triage::emit_triage_applied(&applied_v, &priority_moves, proposed, dropped);
    for err in &locked_errors {
        eprintln!("{err}");
    }
    echo_json(&json!({
        "applied": applied,
        "dropped_due_to_validation": dropped,
    }));
    if !locked_errors.is_empty() {
        return 3;
    }
    0
}

/// Narrow a cleaned proposal to the --pick subset (triage.py _filter_pick):
/// edge keys `from->to`, priority ids, duplicate id-joins, defer ids.
fn filter_pick(cleaned: Value, pick: Option<&BTreeSet<String>>) -> Value {
    let Some(pick) = pick else {
        return cleaned;
    };
    let keep_edge = |d: &Value| {
        let key = format!(
            "{}->{}",
            d["from"].as_str().unwrap_or(""),
            d["to"].as_str().unwrap_or("")
        );
        pick.contains(&key)
    };
    let keep_id = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(|s| pick.contains(s))
            .unwrap_or(false)
    };
    let keep_dups = |d: &Value| {
        let joined = d
            .get("ids")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().map(value_brief).collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        pick.contains(&joined)
    };
    json!({
        "dependencies": cleaned["dependencies"].as_array().map(|rows| rows.iter().filter(|d| keep_edge(d)).cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "priority_changes": cleaned["priority_changes"].as_array().map(|rows| rows.iter().filter(|p| keep_id(p, "id")).cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "duplicates": cleaned["duplicates"].as_array().map(|rows| rows.iter().filter(|d| keep_dups(d)).cloned().collect::<Vec<_>>()).unwrap_or_default(),
        "defer": cleaned["defer"].as_array().map(|rows| rows.iter().filter(|d| keep_id(d, "id")).cloned().collect::<Vec<_>>()).unwrap_or_default(),
    })
}

/// The mutation fold (triage.py cmd_apply's mutator): append missing
/// blocked_by edges, stamp priority moves, and land defers with the same
/// completed_at clearing and exact-match kind classification cmd_defer uses.
fn mutate_locked_entries(
    cleaned: &Value,
    entries: &mut [Value],
    applied: &mut Value,
    priority_moves: &mut Vec<Value>,
) {
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(id) = e.get("id").and_then(Value::as_str) {
            index.insert(id.to_string(), i);
        }
    }
    let empty = Vec::new();
    for edge in cleaned["dependencies"].as_array().unwrap_or(&empty) {
        let (Some(frm), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(ti) = index.get(to).copied() else {
            continue;
        };
        let target = &mut entries[ti];
        let blocked = target
            .get("blocked_by")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if blocked.iter().any(|b| b.as_str() == Some(frm)) {
            continue;
        }
        let obj = target.as_object_mut().unwrap();
        let mut list = blocked;
        list.push(Value::String(frm.to_string()));
        obj.insert("blocked_by".to_string(), Value::Array(list));
        applied["dependencies"] = json!(applied["dependencies"].as_i64().unwrap_or(0) + 1);
    }
    for pc in cleaned["priority_changes"].as_array().unwrap_or(&empty) {
        let (Some(pid), Some(to)) = (pc.get("id").and_then(Value::as_str), pc.get("to").cloned())
        else {
            continue;
        };
        let Some(ti) = index.get(pid).copied() else {
            continue;
        };
        let node = &mut entries[ti];
        let from = node.get("priority").cloned().unwrap_or(Value::Null);
        priority_moves.push(json!({ "id": pid, "from": from, "to": to }));
        node.as_object_mut()
            .unwrap()
            .insert("priority".to_string(), to);
        applied["priority_changes"] = json!(applied["priority_changes"].as_i64().unwrap_or(0) + 1);
    }
    for d in cleaned["defer"].as_array().unwrap_or(&empty) {
        let Some(did) = d.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(reason) = d.get("reason").and_then(Value::as_str) else {
            continue;
        };
        let Some(ti) = index.get(did).copied() else {
            continue;
        };
        let node = &mut entries[ti];
        let obj = node.as_object_mut().unwrap();
        obj.insert("status".to_string(), Value::String("deferred".to_string()));
        obj.insert("completed_at".to_string(), Value::Null);
        obj.insert("deferred_at".to_string(), Value::String(now_iso_utc()));
        obj.insert(
            "deferred_reason".to_string(),
            Value::String(reason.to_string()),
        );
        match crate::backlog::patch::classify_deferred_reason(reason) {
            Some(kind) => {
                obj.insert("deferred_kind".to_string(), Value::String(kind.to_string()));
            }
            None => {
                obj.remove("deferred_kind");
            }
        }
        applied["deferred"] = json!(applied["deferred"].as_i64().unwrap_or(0) + 1);
    }
    let dups = cleaned["duplicates"].as_array().map(Vec::len).unwrap_or(0);
    applied["duplicates_flagged"] = json!(dups as i64);
}

/// The UTC RFC3339 stamp (Z form), the shape every crate writer uses;
/// Python isoformat writes +00:00 but the graph recompute reads both.
fn now_iso_utc() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    crate::provider_cap::epoch_to_rfc3339(now)
}
