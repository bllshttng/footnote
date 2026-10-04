//! `fno backlog add` - the create-a-node surface, ported from
//! `_create_node_impl`/`cmd_add` in `cli/src/fno/graph/cli.py`. Validation
//! order is load-bearing (the first refusal wins), the mutator runs inside
//! one locked write with the rollup resolution, and every post-write step
//! (vote, receipt, dedup, birth hook, readback) is non-fatal except the
//! readback guard.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use super::autolink;
use super::relatedness;
use super::{birth, node_ref};

/// The captured `fno backlog add --help` (typer's rendering, byte for byte);
/// the cut-over wave prints this where the forward used to.
const ADD_HELP: &str = include_str!("create_help.txt");

const DIFFICULTY_HELP: &str = "Intrinsic work difficulty: expected duration, edge cases, unknowns, and a senior tech lead's estimate. Do not use current capacity or model quality.";
const SOURCE_KINDS: [&str; 5] = [
    "organic",
    "from_inbox",
    "from_observation",
    "from_supervisor",
    "operator_request",
];
const VALID_NODE_TYPES: [&str; 4] = ["bug", "epic", "feature", "roadmap"];

#[derive(Debug, Default, Clone)]
pub struct AddArgs {
    pub title: String,
    pub domain: String,
    pub priority: String,
    pub blocks_everything: bool,
    pub difficulty: Option<String>,
    pub blocked_by: Option<String>,
    pub parent: Option<String>,
    pub type_: String,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub roadmap_id: Option<String>,
    pub vision_path: Option<String>,
    pub details: Option<String>,
    pub evidence: Option<String>,
    pub origin_evidence: Option<String>,
    pub description: Option<String>,
    pub size: Option<String>,
    pub batch: Option<String>,
    pub tag: Vec<String>,
    pub source_node: Option<String>,
    pub source_kind: String,
    pub related: Vec<String>,
    pub help: bool,
}

/// `Args` | `Refusal { message, exit }` | `Help`.
pub enum ParsedAdd {
    Args(AddArgs),
    Refusal { message: String, exit: i32 },
    Help,
}

fn flag_name(arg: &str) -> Option<&str> {
    let long = [
        "--domain",
        "--priority",
        "--difficulty",
        "--blocked-by",
        "--parent",
        "--type",
        "--project",
        "--cwd",
        "--roadmap-id",
        "--vision-path",
        "--details",
        "--evidence",
        "--origin-evidence",
        "--description",
        "--size",
        "--batch",
        "--tag",
        "--source-node",
        "--source-kind",
        "--related",
    ];
    if long.contains(&arg) {
        return Some(arg);
    }
    match arg {
        "-p" => Some("--priority"),
        "-t" => Some("--type"),
        "-c" => Some("--cwd"),
        "-d" => Some("--details"),
        "-e" => Some("--evidence"),
        _ => None,
    }
}

fn flag_bool(arg: &str) -> Option<&'static str> {
    match arg {
        "--blocks-everything" => Some("--blocks-everything"),
        _ => None,
    }
}

pub fn parse(tail: &[String]) -> ParsedAdd {
    let mut args = AddArgs {
        domain: "code".into(),
        priority: "p2".into(),
        type_: "feature".into(),
        source_kind: "organic".into(),
        ..Default::default()
    };
    let mut positionals: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let arg = tail[i].as_str();
        i += 1;
        match arg {
            "--help" | "-h" => return ParsedAdd::Help,
            "--blocks-everything" => {
                args.blocks_everything = true;
            }
            _ if flag_bool(arg).is_some() => unreachable!(),
            _ if flag_name(arg).is_some() => {
                let name = flag_name(arg).expect("flag name");
                if name == "--tag" || name == "--related" {
                    // Repeatable; a missing value still consumes the slot.
                    if i < tail.len() {
                        let value = tail[i].clone();
                        i += 1;
                        if name == "--tag" {
                            args.tag.push(value);
                        } else {
                            args.related.push(value);
                        }
                    }
                    continue;
                }
                let Some(value) = tail.get(i) else {
                    return ParsedAdd::Refusal {
                        message: format!("Error: Option '{name}' requires an argument"),
                        exit: 2,
                    };
                };
                i += 1;
                let value = value.clone();
                match name {
                    "--domain" => args.domain = value,
                    "--priority" => args.priority = value,
                    "--difficulty" => args.difficulty = Some(value),
                    "--blocked-by" => args.blocked_by = Some(value),
                    "--parent" => args.parent = Some(value),
                    "--type" => args.type_ = value,
                    "--project" => args.project = Some(value),
                    "--cwd" => args.cwd = Some(value),
                    "--roadmap-id" => args.roadmap_id = Some(value),
                    "--vision-path" => args.vision_path = Some(value),
                    "--details" => args.details = Some(value),
                    "--evidence" => args.evidence = Some(value),
                    "--origin-evidence" => args.origin_evidence = Some(value),
                    "--description" => args.description = Some(value),
                    "--size" => args.size = Some(value),
                    "--batch" => args.batch = Some(value),
                    "--source-node" => args.source_node = Some(value),
                    "--source-kind" => args.source_kind = value,
                    _ => unreachable!("long flags covered"),
                }
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return ParsedAdd::Refusal {
                    message: format!("Error: No such option: {arg}"),
                    exit: 2,
                };
            }
            _ => positionals.push(&tail[i - 1]),
        }
    }
    let Some(title) = positionals.first() else {
        return ParsedAdd::Refusal {
            message: "Error: Missing argument 'TITLE'".into(),
            exit: 2,
        };
    };
    args.title = (*title).clone();
    if positionals.len() > 1 {
        return ParsedAdd::Refusal {
            message: "Error: Got unexpected extra argument(s)".into(),
            exit: 2,
        };
    }
    ParsedAdd::Args(args)
}

pub(crate) struct Refusal {
    pub(crate) message: String,
    pub(crate) exit: i32,
}

/// What [`create_node`] answers: the minted id, or the existing row its
/// `reuse` hook matched inside the same snapshot (nothing was written).
pub(crate) struct Born {
    pub(crate) id: String,
    pub(crate) reused: Option<Value>,
}

fn refused(message: impl Into<String>, exit: i32) -> Refusal {
    Refusal {
        message: message.into(),
        exit,
    }
}

/// The canonical (main) checkout: `git worktree list --porcelain`'s main
/// worktree, else the cwd. A backlog node outlives the worktree it was
/// filed from.
pub(crate) fn repo_root(cwd: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(path) = line.strip_prefix("worktree ") {
                    return path.to_string();
                }
            }
        }
    }
    cwd.to_string_lossy().into_owned()
}

/// Project name -> work-map path lives in settings; the walk also answers
/// detection (path -> name) through the same candidates.
fn detect_project(cwd: &Path) -> Option<String> {
    for doc in super::settings::config_candidates() {
        for (name, raw) in work_pairs(&doc) {
            let expanded = expand_home(&raw).to_string_lossy().into_owned();
            if Path::new(&expanded) == cwd {
                return Some(name);
            }
        }
    }
    None
}

fn work_pairs(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(work) = doc.get("work") else {
        return out;
    };
    if let Some(projects) = work.get("workspaces").and_then(Value::as_object) {
        for ws in projects.values() {
            let Some(rows) = ws.get("projects").and_then(Value::as_array) else {
                continue;
            };
            for row in rows {
                if let (Some(name), Some(path)) = (row.get("name"), row.get("path")) {
                    if let (Some(name), Some(path)) = (name.as_str(), path.as_str()) {
                        out.push((name.to_string(), path.to_string()));
                    }
                }
            }
        }
    }
    if let Some(projects) = work.get("projects").and_then(Value::as_object) {
        for (name, cfg) in projects {
            if let Some(path) = cfg.get("path").and_then(Value::as_str) {
                out.push((name.clone(), path.to_string()));
            }
        }
    }
    out
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// The shared priority gate: PRIORITY_ORDER membership, then the write rule.
/// Exit 1 is the membership miss, exit 2 the write rule.
fn validate_priority(priority: &str, blocks_everything: bool) -> Result<(), Refusal> {
    if !["p0", "p1", "p2", "p3"].contains(&priority) {
        return Err(refused(
            format!("Error: invalid priority '{priority}'. Must be: p0, p1, p2, p3"),
            1,
        ));
    }
    if priority == "p0" && !blocks_everything {
        return Err(refused(
            "Error: p0 blocks everything else, usually a bug. Use --blocks-everything \
             only when a broken service or fleet-wide resource leak blocks all \
             downstream work. If it does, say so with --blocks-everything; otherwise \
             file it p1.",
            2,
        ));
    }
    Ok(())
}

fn normalize_difficulty(value: &str) -> Result<String, Refusal> {
    let band = value.trim().to_lowercase();
    if !["low", "medium", "high"].contains(&band.as_str()) {
        return Err(refused(
            format!(
                "Error: invalid difficulty '{value}'; must be one of: low, medium, high. {DIFFICULTY_HELP}"
            ),
            2,
        ));
    }
    Ok(band)
}

fn normalize_tag(raw: &str) -> Result<String, Refusal> {
    let tag = raw.trim().to_lowercase();
    let ok = !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(refused(
            format!(
                "Error: invalid tag '{raw}': tags must be lowercase-kebab [a-z0-9-] (letters, digits, hyphens)"
            ),
            1,
        ));
    }
    Ok(tag)
}

/// The external-backend guard fired before any read or write: creation of
/// graph-owned rows belongs to the default backend only.
fn refuse_tracker_owned(label: &str) -> Result<(), Refusal> {
    let backend = active_backend_name();
    if backend != "graph" {
        return Err(refused(
            format!(
                "fno backlog {label}: this verb owns graph state; under the {backend} \
                 tracker backend it is refused. Track the item in the tracker by its id."
            ),
            1,
        ));
    }
    Ok(())
}

fn sorted_source_kinds() -> String {
    let mut kinds = SOURCE_KINDS.to_vec();
    kinds.sort();
    kinds.join(", ")
}

fn active_backend_name() -> String {
    std::env::var("FNO_TRACKER_BACKEND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "graph".to_string())
}

/// One birth record through the native owner, in-process. Returns
/// `(origin, evidence_ref, refused)`; the caller refuses operator_request
/// births the origin did not grant.
fn stamp_request_origin(
    source_kind: &str,
    origin_evidence: Option<&str>,
) -> (String, Option<String>, Option<String>) {
    let record = crate::node_origin::BirthRecord {
        request_origin: None,
        source_kind: Some(source_kind.to_string()),
        birth_channel: Some("idea".to_string()),
        origin_evidence: origin_evidence.map(str::to_string),
    };
    let pending = if record.source_kind.as_deref() == Some("operator_request") {
        crate::operator_turns::session_queue_depth(
            &|key| std::env::var(key).ok(),
            &crate::paths::AgentsHome::from_env(),
            &std::env::current_dir().unwrap_or_default(),
            chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        )
    } else {
        Ok(0)
    };
    let resolution = crate::node_origin::resolve(&record, &pending);
    (
        resolution.origin.as_str().to_string(),
        resolution.evidence,
        resolution.refused,
    )
}

/// Parent-edge provenance for a node born inside a live session. Every key
/// degrades to None and this never fails. Origin precedence: an explicit
/// --source-node, then the owned manifest (claude-only), then FNO_NODE.
pub(crate) fn session_provenance(
    cwd: &Path,
    source_node: Option<&str>,
    known_ids: Option<&std::collections::BTreeSet<String>>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let get = |name: &str| std::env::var(name).ok();
    let ident = crate::spawn_context::resolve_self_identity(
        &get,
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let mut session = ident.session_id.clone().filter(|s| !s.trim().is_empty());
    let mut harness = ident.harness.clone().filter(|s| !s.trim().is_empty());
    if session.is_none() {
        // The Python identity's vendor fallback: a lone ambient marker names
        // its harness and session without proof (single family, one value).
        // Two families or two ids of one family stay unresolved.
        let markers: [(&str, &str); 6] = [
            ("CODEX_THREAD_ID", "codex"),
            ("CLAUDE_CODE_SESSION_ID", "claude"),
            ("CODEX_SESSION_ID", "codex"),
            ("GEMINI_SESSION_ID", "gemini"),
            ("OPENCODE_SESSION_ID", "opencode"),
            ("CLAUDE_SESSION_ID", "claude"),
        ];
        let mut winner: Option<(&str, String)> = None;
        let mut conflicted = false;
        for (marker, family) in markers {
            let Some(value) = get(marker)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            else {
                continue;
            };
            if winner.is_none() {
                winner = Some((family, value));
            } else if let Some((f, v)) = &winner {
                if f != &family || v != &value {
                    conflicted = true;
                }
            }
        }
        if !conflicted {
            if let Some((family, value)) = winner {
                harness = Some(family.to_string());
                session = Some(value);
            }
        }
    }

    let mut source_node_id: Option<String> = None;
    let mut source_plan_path: Option<String> = None;
    if session.is_some() && harness.as_deref() == Some("claude") {
        if let Ok(text) = std::fs::read_to_string(cwd.join(".fno").join("target-state.md")) {
            let manifest_sid = birth::scan_md_field(&text, "claude_session_id")
                .or_else(|| birth::scan_md_field(&text, "claude_transcript_id"));
            if manifest_sid.as_deref() == session.as_deref() {
                let nid = birth::scan_md_field(&text, "graph_node_id")
                    .filter(|v| !v.eq_ignore_ascii_case("null"));
                source_node_id = nid;
                let plan = birth::scan_md_field(&text, "plan_path")
                    .filter(|v| !v.eq_ignore_ascii_case("null"));
                source_plan_path = plan;
            }
        }
    }
    if source_node_id.is_none() {
        source_node_id = std::env::var("FNO_NODE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
    }

    let mut dropped: Option<String> = None;
    if let (Some(id), Some(known)) = (&source_node_id, known_ids) {
        if !known.contains(id) {
            dropped = source_node_id.take();
        }
    }
    if let Some(explicit) = source_node {
        source_node_id = Some(explicit.to_string());
        dropped = None;
    }

    let source_cwd = session.as_ref().map(|_| cwd.to_string_lossy().into_owned());
    (
        session,
        harness,
        source_cwd,
        source_node_id,
        source_plan_path,
        dropped,
    )
}

/// Mint a fresh, collision-free node id against the snapshot's ids and the
/// legacy archive pool. Must run inside the locked write.
fn mint_node_id(existing: &std::collections::BTreeSet<String>) -> Result<String, String> {
    let prefix = super::settings::node_id_prefix();
    let width = super::settings::node_id_hex_width();
    let archive_ids = archived_id_pool();
    mint_node_id_with_prefix(existing, &archive_ids, &prefix, width)
}

fn mint_node_id_with_prefix(
    existing: &std::collections::BTreeSet<String>,
    archive_ids: &std::collections::BTreeSet<String>,
    prefix: &str,
    width: usize,
) -> Result<String, String> {
    let prefix = if prefix.ends_with('-') {
        prefix.to_string()
    } else {
        format!("{prefix}-")
    };
    for _ in 0..64 {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|e| format!("mint entropy failed: {e}"))?;
        let candidate = format!("{prefix}{}", &hex_lower(&bytes)[..width]);
        if !existing.contains(&candidate) && !archive_ids.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err("node-ID space near exhaustion after 64 mint attempts".to_string())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The legacy graph-archive.json id pool, best-effort. The store snapshot
/// already covers archived rows; this file only matters on installs that
/// still keep the pre-store archive.
fn archived_id_pool() -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let Some(dir) = super::settings::state_dir() else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(dir.join("graph-archive.json")) else {
        return out;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    if let Some(entries) = value.get("entries").and_then(Value::as_array) {
        for e in entries {
            if let Some(id) = e.get("id").and_then(Value::as_str) {
                out.insert(id.to_string());
            }
        }
    }
    out
}

/// The dedup net: score the just-born node against every live node plus the
/// archived residents. Warn-only; stderr keeps stdout machine-readable.
fn warn_similar_nodes(node: &Value, entries: &[Value], minted_id: &str) {
    let candidates = relatedness::similar_nodes(node, entries, 3, None);
    if candidates.is_empty() {
        return;
    }
    let mut lines = vec![format!(
        "dedup: {count} similar existing node(s) for {minted_id}:",
        count = candidates.len()
    )];
    for (cid, score, _reason) in &candidates {
        let cand = entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(cid.as_str()));
        let status = cand
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let mut title = cand
            .and_then(|c| c.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if title.chars().count() > 60 {
            title = title.chars().take(60).collect::<String>() + "...";
        }
        let pr = cand
            .and_then(|c| c.get("pr_number"))
            .and_then(Value::as_i64);
        let pr_tok = pr.map(|n| format!("  PR#{n}")).unwrap_or_default();
        lines.push(format!(
            "  {cid}  {status:<10}{score:.2}{pr_tok}  \"{title}\""
        ));
    }
    lines.push(
        "consolidate: `fno backlog supersede` / `fno backlog update` the existing node".to_string(),
    );
    eprintln!("{}", lines.join("\n"));
}

/// The creation vote: best-effort creator encounter after the node lands.
/// Style gate, identity, and the same-voter duplicate refusal ride the
/// append. Filing stays primary: every failure prints one warning.
fn append_creation_vote(graph: &Path, node_id: &str, evidence: &str) {
    let warn = |why: &str| eprintln!("warning: creation vote skipped for {node_id}: {why}");
    let evidence = evidence.trim();
    if evidence.is_empty() {
        warn("an encounter with no evidence is a poll");
        return;
    }
    if std::env::var("FNO_STYLE_ENFORCE").as_deref() != Ok("0")
        && super::style_check::has_exception(evidence).is_none()
    {
        let cap = super::settings::config_candidates()
            .iter()
            .find_map(|doc| {
                doc.get("style")?
                    .get("word_cap")?
                    .get("encounter")?
                    .as_u64()
            })
            .map(|n| n as usize);
        let violations = super::style_check::check(evidence, "encounter", cap);
        if !violations.is_empty() {
            warn(&super::style_check::format_violations(
                &violations,
                "encounter",
            ));
            return;
        }
    }
    let get = |name: &str| std::env::var(name).ok();
    let ident = crate::spawn_context::resolve_self_identity(
        &get,
        None,
        None,
        &crate::paths::AgentsHome::from_env(),
    );
    let session_id = ident.session_id.clone().filter(|s| !s.trim().is_empty());
    let harness = ident.harness.clone().filter(|s| !s.trim().is_empty());
    let (Some(session_id), Some(harness)) = (session_id, harness) else {
        warn("no provable session identity");
        return;
    };
    let mut record = Map::new();
    record.insert(
        "ts".into(),
        Value::String(crate::graph_store::now_isoformat()),
    );
    record.insert("session_id".into(), Value::String(session_id.clone()));
    record.insert("voter_key".into(), Value::String(session_id.clone()));
    record.insert("voter_kind".into(), Value::String("agent".into()));
    record.insert("harness".into(), Value::String(harness.clone()));
    record.insert(
        "fno_id".into(),
        Value::String(canonical_handle(&session_id)),
    );
    record.insert("evidence".into(), Value::String(evidence.to_string()));
    // Model/effort provenance for a claude session, best-effort.
    if harness == "claude" {
        if let Ok(effort) = std::env::var("CLAUDE_EFFORT") {
            let effort = effort.trim().to_string();
            if !effort.is_empty() {
                record.insert("effort".into(), Value::String(effort));
            }
        }
        if std::env::var("ANTHROPIC_BASE_URL").is_ok_and(|v| !v.trim().is_empty()) {
            if let Ok(model) = std::env::var("ANTHROPIC_MODEL") {
                let model = model.trim().to_string();
                if !model.is_empty() {
                    record.insert("model".into(), Value::String(model));
                }
            }
        }
    }
    // The append: one locked write with the keeper's same-voter refusal.
    let run = || -> Result<(), String> {
        let base_version = crate::graph_store::base_version(graph).map_err(|e| e.to_string())?;
        let mut entries = crate::graph_store::read_rows(graph).map_err(|e| e.to_string())?;
        let Some(row) = entries
            .iter_mut()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(node_id))
        else {
            return Err(format!("no node resolves to '{node_id}'"));
        };
        let key = session_id.clone();
        if let Some(obj) = row.as_object_mut() {
            let existing = obj
                .get("encounters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for prior in &existing {
                let prior_key = prior
                    .get("voter_key")
                    .and_then(Value::as_str)
                    .or_else(|| prior.get("session_id").and_then(Value::as_str))
                    .unwrap_or_default();
                if prior_key == key {
                    return Err(format!(
                        "voter {key} already recorded an encounter on {node_id} at {}",
                        prior
                            .get("created_at")
                            .or_else(|| prior.get("ts"))
                            .and_then(Value::as_str)
                            .unwrap_or("None")
                    ));
                }
                let _ = prior;
            }
            let encounters = obj
                .entry("encounters".to_string())
                .or_insert_with(|| Value::Array(vec![]));
            if !encounters.is_array() {
                *encounters = Value::Array(vec![]);
            }
            encounters
                .as_array_mut()
                .expect("just made an array")
                .push(Value::Object(record.clone()));
        }
        crate::graph_store::locked_mutate(
            graph,
            crate::graph_store::MutateInput {
                entries,
                canonical_path: None,
                base_version,
                plan_rungs: None,
            },
            crate::graph_store::DEFAULT_LOCK_TIMEOUT,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    };
    if let Err(e) = run() {
        warn(&e);
    }
}

fn canonical_handle(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// The add flow end to end: parse, gate order, locked write with the rollup
/// inside it, then the post-write ladder. Every post step is non-fatal
/// except the readback guard.
pub fn run(tail: &[String]) -> i32 {
    let args = match parse(tail) {
        ParsedAdd::Help => {
            print!("{ADD_HELP}");
            return 0;
        }
        ParsedAdd::Refusal { message, exit } => {
            eprintln!("{message}");
            return exit;
        }
        ParsedAdd::Args(a) => a,
    };
    match create_node(&args, &|_| None) {
        Ok(born) => {
            println!(
                "{}",
                super::render::py_json_pretty(&json!({"id": born.id, "title": args.title}))
            );
            0
        }
        Err(r) => {
            eprintln!("{}", r.message);
            r.exit
        }
    }
}

/// The create flow without the stdout receipt. `reuse` runs over every
/// fresh snapshot the write loop reads, before the mint: a row it returns
/// wins and nothing is written. Because a lost publish race re-reads and
/// re-asks, two concurrent identical births end with one node.
pub(crate) fn create_node(
    args: &AddArgs,
    reuse: &dyn Fn(&[Value]) -> Option<Value>,
) -> Result<Born, Refusal> {
    refuse_tracker_owned("add")?;
    validate_priority(&args.priority, args.blocks_everything)?;
    let difficulty = match &args.difficulty {
        None => {
            return Err(refused(
                format!(
                    "Error: non-interactive filing requires --difficulty (low, medium, high). {DIFFICULTY_HELP}"
                ),
                2,
            ));
        }
        Some(d) => normalize_difficulty(d)?,
    };
    if !VALID_NODE_TYPES.contains(&args.type_.as_str()) {
        return Err(refused(
            format!(
                "Error: invalid type '{}'. Must be one of: {}",
                args.type_,
                VALID_NODE_TYPES.join(", ")
            ),
            1,
        ));
    }
    if !SOURCE_KINDS.contains(&args.source_kind.as_str()) {
        return Err(refused(
            format!(
                "Error: invalid --source-kind '{}'. Must be one of: {}",
                args.source_kind,
                sorted_source_kinds()
            ),
            1,
        ));
    }
    if args.details.is_some() && args.description.is_some() {
        return Err(refused(
            "Error: pass --details or --description, not both",
            1,
        ));
    }
    let details = args.details.clone().or_else(|| args.description.clone());
    let mut resolved_tags: Vec<String> = Vec::new();
    for t in &args.tag {
        resolved_tags.push(normalize_tag(t)?);
    }
    let mut dedup_tags: Vec<String> = Vec::new();
    for t in resolved_tags {
        if !dedup_tags.contains(&t) {
            dedup_tags.push(t);
        }
    }
    let (origin, origin_evidence_ref, refused_note) =
        stamp_request_origin(&args.source_kind, args.origin_evidence.as_deref());
    if args.source_kind == "operator_request" && origin != "operator_request" {
        return Err(refused(
            format!("Error: {}", refused_note.unwrap_or_default()),
            1,
        ));
    }

    // Store an absolute path so downstream detection finds matches. No
    // explicit --cwd: record the canonical main checkout, not os.getcwd().
    let cwd_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resolved_cwd: String = match &args.cwd {
        Some(cwd) => {
            let p = Path::new(cwd);
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd_root.join(p)
            };
            joined.to_string_lossy().into_owned()
        }
        None => match &args.project {
            Some(project) => {
                super::settings::project_root(project).unwrap_or_else(|| repo_root(&cwd_root))
            }
            None => repo_root(&cwd_root),
        },
    };
    let resolved_project = args
        .project
        .clone()
        .or_else(|| detect_project(&PathBuf::from(&resolved_cwd)));

    // One snapshot read feeds the fail-closed assertions, the mint, and the
    // mutation: an unresolvable assertion must leave the graph untouched. The
    // write cycle retries on conflict the way the keeper's op loop does: a
    // peer publish between this read and the lock re-runs the whole decision
    // body (mint, rollup resolution, related edges) over a fresh snapshot,
    // so two concurrent filings both persist.
    let graph = super::settings::graph_path();
    let mut retry = 0u8;
    let (minted, node, rollup_lines, dropped, source_node_id) = loop {
        retry += 1;
        let base_version = crate::graph_store::base_version(&graph)
            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
        let rows = crate::graph_store::read_rows(&graph)
            .map_err(|e| refused(format!("graph read failed: {e}"), 1))?;
        let known_ids: std::collections::BTreeSet<String> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if let Some(row) = reuse(&rows) {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Ok(Born {
                id,
                reused: Some(row),
            });
        }

        // Fail closed BEFORE minting: an unresolvable assertion names the flag.
        let resolved_source_node: Option<String> = match &args.source_node {
            Some(token) => {
                let resolved = node_ref::resolve_asserted_id(token, &rows, "--source-node", None)
                    .map_err(|e| refused(e.0, e.1))?;
                Some(resolved)
            }
            None => None,
        };

        let minted = mint_node_id(&known_ids).map_err(|e| refused(e, 1))?;

        // Build the node (the builder's field set, byte-shaped like Python's).
        let now = crate::graph_store::now_isoformat();
        let provenance =
            session_provenance(&cwd_root, resolved_source_node.as_deref(), Some(&known_ids));
        let (
            source_session_id,
            source_harness,
            source_cwd,
            source_node_id,
            source_plan_path,
            dropped,
        ) = provenance;
        // The creating session's registry row AT BIRTH: the lane facts the
        // activity feed's provenance view prints for a node_created row. A
        // session no registry row holds (a human filing by hand) reads as
        // absent, never guessed.
        let (source_model, source_effort, source_parent, source_team) = source_session_id
            .as_deref()
            .and_then(|sid| {
                let registry = crate::state::load_registry(
                    &crate::paths::AgentsHome::from_env().registry_json(),
                )
                .ok()?;
                registry
                    .entries
                    .iter()
                    .find(|e| e.harness_session_id.as_deref() == Some(sid))
                    .map(|e| {
                        (
                            e.model.clone(),
                            e.effort.clone(),
                            e.spawned_by_session.clone(),
                            e.crown_level
                                .zip(e.crown_scope.clone())
                                .map(|(level, scope)| format!("L{level} {scope}")),
                        )
                    })
            })
            .unwrap_or((None, None, None, None));
        let mut node = json!({
            "id": minted,
            "parent": args.parent,
            "tags": dedup_tags,
            "title": args.title,
            "type": args.type_,
            "project": resolved_project,
            "cwd": resolved_cwd,
            // Plan-less birth: the derived status the Python write stored.
            "status": "idea",
            "priority": args.priority,
            "blocks_everything": args.blocks_everything,
            "difficulty": difficulty,
            "difficulty_history": [{"value": difficulty, "source": "filed", "ts": now}],
            "domain": args.domain,
            "blocked_by": parse_blocker_list(args.blocked_by.as_deref()),
            "session_id": Value::Null,
            "locked_at": Value::Null,
            "completed_at": Value::Null,
            "has_brief": false,
            "roadmap_id": args.roadmap_id,
            "vision_path": args.vision_path,
            "details": details,
            "size": args.size,
            "batch": args.batch,
            "cost_usd": Value::Null,
            "cost_sessions": [],
            "plan_path": Value::Null,
            "pr_number": Value::Null,
            "pr_url": Value::Null,
            "merge_status": Value::Null,
            "created_at": now,
            "source": Value::Null,
            "source_kind": args.source_kind,
            "source_project": Value::Null,
            "source_inbox_msg": Value::Null,
            "artifact_url": Value::Null,
            "completion_note": Value::Null,
            "source_session_id": source_session_id,
            "source_harness": source_harness,
            "source_model": source_model,
            "source_effort": source_effort,
            "source_parent_session": source_parent,
            "source_team": source_team,
            "source_cwd": source_cwd,
            "source_node_id": source_node_id,
            "source_plan_path": source_plan_path,
            "request_origin": origin,
            "origin_evidence": origin_evidence_ref,
        });

        // Refuse a birth --parent that cannot hold the child; the scope and
        // lenient pass-through cases are named in strand.birth_parent_refusal.
        if let Some(parent) = &args.parent {
            if let Some(message) = birth_parent_refusal(&rows, &node, parent) {
                return Err(refused(message, 1));
            }
        }

        // Rollup resolution reads the same snapshot the node was born into and
        // its parent edge lands in the SAME write, so no window exists where the
        // node is linked without a receipt. Strictly non-fatal: any failure
        // degrades to the orphan line.
        let mut working = rows.clone();
        let rollup_lines: Vec<String>;
        {
            let mut entries = working.clone();
            entries.push(node.clone());
            let resolution = autolink::resolve(&node, &entries, None);
            rollup_lines = autolink::receipt_lines(&resolution, &minted, &entries);
            if matches!(resolution.kind, "linked" | "team") {
                if let Some(epic_id) = &resolution.epic_id {
                    // The edge lands on the row INSIDE the write, so the node
                    // never exists linked-without-receipt (or vice versa).
                    if let Some(obj) = entries.last_mut().and_then(Value::as_object_mut) {
                        obj.insert("parent".into(), Value::String(epic_id.clone()));
                    }
                    if let Some(obj) = node.as_object_mut() {
                        obj.insert("parent".into(), Value::String(epic_id.clone()));
                    }
                }
            }
            working = entries;
        }

        // The symmetric related edges, resolved against the mint's own snapshot
        // (self-references refuse with the flag's name).
        if !args.related.is_empty() {
            let mut tokens: Vec<String> = Vec::new();
            for token in parse_blocker_list_args(&args.related) {
                let resolved =
                    node_ref::resolve_asserted_id(&token, &working, "--related", Some(&minted))
                        .map_err(|e| refused(e.0, e.1))?;
                tokens.push(resolved);
            }
            let _ = crate::graph_keeper::set_related(&mut working, &minted, &tokens);
        }

        // The shared write pipeline recomputes statuses across the graph on
        // every commit (a plan-less filing's stored status lands as idea), the
        // same recompute commit_rows_via_store ran.
        crate::graph_store::recompute_statuses_with_plan_rungs(&mut working, None);
        match crate::graph_store::locked_mutate(
            &graph,
            crate::graph_store::MutateInput {
                entries: working,
                canonical_path: None,
                base_version,
                plan_rungs: None,
            },
            crate::graph_store::DEFAULT_LOCK_TIMEOUT,
        ) {
            Ok(_) => break (minted, node, rollup_lines, dropped, source_node_id),
            Err(crate::graph_store::StoreError::Conflict) if retry < 5 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(refused(format!("graph write failed: {e}"), 1)),
        }
    };

    // The creation vote lands right after the write, before the receipts.
    if let Some(evidence) = &args.evidence {
        append_creation_vote(&graph, &minted, evidence);
    }
    // stderr, not stdout: stdout is a machine-readable JSON payload callers
    // pipe through jq. Every receipt here is advisory human output.
    for line in &rollup_lines {
        super::receipt::emit_line_to(std::io::stderr(), line);
    }
    // Name a captured origin on stderr. Silent when no signal existed at
    // all, never silent when one was found and rejected.
    if let Some(dropped) = &dropped {
        eprintln!("origin: dropped '{dropped}' (not in graph); filed with no origin");
    } else if let Some(sid) = &source_node_id {
        eprintln!("origin: {sid}");
    }
    // Filing-time dedup net, fresh read, non-fatal. The corpus adds the
    // archived residents: a shipped done node is the answer to a duplicate.
    {
        if let Ok(mut post_rows) = crate::graph_store::read_rows(&graph) {
            // The pydantic read derives the single-entry status; the dedup
            // lines name the same derived word the live readers present.
            for row in post_rows.iter_mut() {
                if let Some(obj) = row.as_object_mut() {
                    let derived = derive_status(obj, &graph);
                    obj.insert("status".into(), Value::String(derived));
                }
            }
            if let Some(born) = post_rows
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(minted.as_str()))
            {
                warn_similar_nodes(born, &post_rows, &minted);
            }
        }
    }
    // Born-with-why, gate-first and strictly non-fatal.
    let _ = birth::on_node_born(&graph, &node, None);
    // Repaint the new child's ancestors so a parent rollup reflects the
    // birth immediately (a plan-less idea still counts toward children_total).
    if let Some(parent) = node.get("parent").and_then(Value::as_str) {
        if !parent.is_empty() {
            if let Ok(rows_now) = crate::graph_store::read_rows(&graph) {
                crate::plan_doc::project::project_graph_nodes(
                    &rows_now,
                    std::slice::from_ref(&minted),
                    None,
                    None,
                    None,
                    None,
                );
            }
        }
    }
    // The read-back guard: a write the store cannot confirm is an error.
    let readback = crate::graph_store::read_rows(&graph).ok().and_then(|rows| {
        rows.iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(minted.as_str()))
            .cloned()
    });
    match readback {
        None => {
            return Err(refused(
                format!(
                    "Error: filed {minted} but the read-back could not confirm it (store read failed); verify before re-filing"
                ),
                1,
            ));
        }
        Some(row) if row.is_null() => {
            return Err(refused(
                format!(
                    "Error: filed {minted} but it does not read back from the store; the write did not land"
                ),
                1,
            ));
        }
        _ => {}
    }
    Ok(Born {
        id: minted,
        reused: None,
    })
}

/// The single-entry status ladder the Python model derives on every read:
/// terminal fields, then PR, blockers, lock, then the plan rung (no plan ->
/// idea; unreadable plan or a plan-side terminal -> ready; design -> design).
fn derive_status(entry: &serde_json::Map<String, Value>, _graph: &Path) -> String {
    if entry.get("completed_at").is_some_and(|v| !v.is_null()) {
        return "done".into();
    }
    if entry.get("superseded_by").is_some_and(|v| !v.is_null()) {
        return "superseded".into();
    }
    if entry.get("deferred_at").is_some_and(|v| !v.is_null()) {
        return "deferred".into();
    }
    if entry.get("pr_number").is_some_and(|v| !v.is_null()) {
        return "in_review".into();
    }
    if entry
        .get("blocked_by")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        return "blocked".into();
    }
    if ["locked_by", "session_id"].iter().any(|k| {
        entry
            .get(*k)
            .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    }) {
        return "in_progress".into();
    }
    let Some(plan_path) = entry
        .get("plan_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    else {
        return "idea".into();
    };
    let resolved = match Path::new(plan_path).is_absolute() {
        true => PathBuf::from(plan_path),
        false => {
            let cwd = entry.get("cwd").and_then(Value::as_str).unwrap_or_default();
            if cwd.is_empty() {
                // Repo-relative with no anchor: cannot tell, fail open.
                return "ready".into();
            }
            Path::new(cwd).join(plan_path)
        }
    };
    let Ok(text) = std::fs::read_to_string(&resolved) else {
        return "ready".into();
    };
    match plan_frontmatter_status(&text) {
        Some(word) => match word.as_str() {
            "design" => "design".into(),
            "idea" => "idea".into(),
            // Plan-side terminals map to ready: graph truth for those is
            // completed_at/pr_number/superseded_by, never the doc itself.
            _ => "ready".into(),
        },
        // Readable, but declares no status: ready, the pre-ladder answer.
        None => "ready".into(),
    }
}

/// The first `status:` scalar in a leading frontmatter block, if any.
fn plan_frontmatter_status(text: &str) -> Option<String> {
    let mut in_block = false;
    for line in text.lines() {
        let lead = line.trim();
        if lead == "---" {
            if in_block {
                break;
            }
            in_block = true;
            continue;
        }
        if in_block {
            if let Some(rest) = lead.strip_prefix("status:") {
                let value = rest
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Refusal line for a birth `--parent` that cannot hold the child, else
/// None. A closed parent refuses because the healers clear that edge after
/// birth; an unresolvable parent keeps the lenient birth pass-through.
fn birth_parent_refusal(entries: &[Value], node: &Value, parent: &str) -> Option<String> {
    const EPIC_NEST_MAX_DEPTH: usize = 2;
    let target = node_ref::find_node(entries, parent)?;
    let status = target
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(status, "done" | "superseded" | "deferred") {
        return Some(format!(
            "Error: --parent {} is {status}; the next reconcile re-parents children of closed \
             nodes, so the edge would not survive. File under a live parent, or without --parent.",
            target.get("id").and_then(Value::as_str).unwrap_or_default()
        ));
    }
    if node_ref::would_exceed_epic_depth(entries, node, target) {
        return Some(format!(
            "Error: parenting epic {} under {} exceeds the {EPIC_NEST_MAX_DEPTH}-level cap \
             (mission -> epic -> leaf); an epic nests only under a mission",
            node.get("id").and_then(Value::as_str).unwrap_or_default(),
            target.get("id").and_then(Value::as_str).unwrap_or_default()
        ));
    }
    None
}

fn parse_blocker_list_args(values: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for v in values {
        for token in v.split(',') {
            let t = token.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_id_prefix_mints_dash_before_hex() {
        let minted = mint_node_id_with_prefix(
            &std::collections::BTreeSet::new(),
            &std::collections::BTreeSet::new(),
            "x",
            4,
        )
        .expect("minted id");

        assert!(minted.starts_with("x-"));
        assert_eq!(minted.len(), 6);
        assert!(minted[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_reads_long_short_and_repeatable_flags() {
        let tail: Vec<String> = [
            "T",
            "--difficulty",
            "low",
            "-p",
            "p0",
            "--blocks-everything",
            "--tag",
            "a-b",
            "--tag",
            "c-d",
            "--related",
            "x-f00d0003",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = match parse(&tail) {
            ParsedAdd::Args(a) => a,
            _ => panic!("parses"),
        };
        assert_eq!(a.title, "T");
        assert_eq!(a.difficulty.as_deref(), Some("low"));
        assert_eq!(a.priority, "p0");
        assert!(a.blocks_everything);
        assert_eq!(a.tag, vec!["a-b".to_string(), "c-d".to_string()]);
        assert_eq!(a.related, vec!["x-f00d0003".to_string()]);

        // A missing title refuses like typer.
        let untitled: Vec<String> = ["--difficulty", "low"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse(&untitled) {
            ParsedAdd::Refusal { exit, .. } => assert_eq!(exit, 2),
            _ => panic!("refuses"),
        }
    }

    #[test]
    fn derive_status_matches_the_ladder() {
        let base = serde_json::Map::new();
        assert_eq!(derive_status(&base, Path::new("/x")), "idea");
        let mut locked = serde_json::Map::new();
        locked.insert("locked_by".into(), Value::String("w".into()));
        assert_eq!(derive_status(&locked, Path::new("/x")), "in_progress");
        let mut blocked = serde_json::Map::new();
        blocked.insert(
            "blocked_by".into(),
            Value::Array(vec![Value::String("x-1".into())]),
        );
        assert_eq!(derive_status(&blocked, Path::new("/x")), "blocked");
        let mut done = serde_json::Map::new();
        done.insert("completed_at".into(), Value::String("t".into()));
        assert_eq!(derive_status(&done, Path::new("/x")), "done");
    }
}

fn parse_blocker_list(value: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(v) = value {
        for token in v.split(',') {
            let t = token.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// The holder of a live/suspect `node:<id>` claim, else None. A suspect
/// claim (TTL unexpired, pid dead) still belongs to its session, so it
/// counts as a worker here too.
pub(crate) fn live_worker(node_id: &str) -> Option<String> {
    let key = format!("node:{node_id}");
    let Ok(path) = crate::claims::claim_path(&key, None) else {
        return None;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return None;
    };
    let Ok(rec) = serde_json::from_str::<crate::claims::ClaimRecord>(&text) else {
        return None;
    };
    match crate::claims::classify(&rec, None) {
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect => Some(rec.holder),
        _ => None,
    }
}

/// The idea-only flag tail on top of add's surface: wave filing, the fold
/// gate's escape, and the JSON receipt.
#[derive(Debug, Default)]
pub struct IdeaArgs {
    pub add: AddArgs,
    pub wave_of: Option<String>,
    pub separate: bool,
    pub json_out: bool,
    pub details_file: Option<String>,
}

pub enum ParsedIdea {
    Args(IdeaArgs),
    Refusal { message: String, exit: i32 },
    Help,
}

pub fn parse_idea(tail: &[String]) -> ParsedIdea {
    let mut idea = IdeaArgs::default();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let arg = tail[i].as_str();
        match arg {
            "--wave-of" => {
                let Some(v) = tail.get(i + 1) else {
                    return ParsedIdea::Refusal {
                        message: "Error: Option '--wave-of' requires an argument".into(),
                        exit: 2,
                    };
                };
                idea.wave_of = Some(v.clone());
                i += 2;
            }
            "--separate" => {
                idea.separate = true;
                i += 1;
            }
            "--json" | "-J" => {
                idea.json_out = true;
                i += 1;
            }
            "--details-file" => {
                let Some(v) = tail.get(i + 1) else {
                    return ParsedIdea::Refusal {
                        message: "Error: Option '--details-file' requires an argument".into(),
                        exit: 2,
                    };
                };
                idea.details_file = Some(v.clone());
                i += 2;
            }
            _ => {
                rest.push(tail[i].clone());
                i += 1;
            }
        }
    }
    match parse(&rest) {
        ParsedAdd::Help => ParsedIdea::Help,
        ParsedAdd::Refusal { message, exit } => ParsedIdea::Refusal { message, exit },
        ParsedAdd::Args(add) => {
            idea.add = add;
            ParsedIdea::Args(idea)
        }
    }
}

/// Read --details from a file ("-" = stdin) when --details-file names one.
/// The refusal texts are the shared seam's (fno/text_or_file.py), which the
/// intake and capture verbs still serve: the twin must stay byte-equal.
fn read_text_arg(
    details: &Option<String>,
    details_file: &Option<String>,
) -> Result<Option<String>, Refusal> {
    if details_file.is_none() {
        return Ok(details.clone());
    }
    if details.is_some() {
        return Err(refused(
            "error: provide the details once - inline or as a file, not both",
            1,
        ));
    }
    let path = details_file.clone().expect("checked");
    let body = if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| refused(format!("error: cannot read -: {e}"), 1))?;
        buf
    } else {
        std::fs::read_to_string(&path)
            .map_err(|e| refused(format!("error: cannot read {path}: {e}"), 1))?
    };
    Ok(Some(body))
}

/// shlex.quote: wrap in single quotes when the token is not shell-safe.
fn shlex_quote(token: &str) -> String {
    if !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_%+=:,@./-".contains(&b))
    {
        return token.to_string();
    }
    format!("'{}'", token.replace('\'', "'\\''"))
}

/// Append the wave note to the target's progress_notes under one locked
/// write, with the keeper's same terminal and missing-target refusals.
fn file_wave(
    json_out: bool,
    target_id: &str,
    title: &str,
    body: Option<&str>,
    difficulty: &str,
    source_node: Option<&str>,
) -> Result<(), Refusal> {
    let text = body.unwrap_or(title).to_string();
    let mut source = source_node
        .map(str::to_string)
        .or_else(|| std::env::var("FNO_NODE").ok())
        .unwrap_or_else(|| "fno backlog idea".to_string());
    if source.trim().is_empty() {
        source = "fno backlog idea".to_string();
    }
    let note = json!({
        "ts": crate::graph_store::now_isoformat(),
        "kind": "wave",
        "title": title,
        "details": body,
        "difficulty": difficulty,
        "source": source,
        "text": text,
    });
    let graph = super::settings::graph_path();
    let run = || -> Result<(), String> {
        let base_version = crate::graph_store::base_version(&graph).map_err(|e| e.to_string())?;
        let mut entries = crate::graph_store::read_rows(&graph).map_err(|e| e.to_string())?;
        let Some(idx) = entries
            .iter()
            .position(|r| r.get("id").and_then(Value::as_str) == Some(target_id))
        else {
            return Err(format!("no node resolves to '{target_id}'"));
        };
        {
            let obj = entries[idx].as_object_mut().expect("row is an object");
            let stored_done = matches!(
                obj.get("status").and_then(Value::as_str),
                Some("done") | Some("superseded")
            );
            let completed = obj
                .get("completed_at")
                .map(|v| !v.is_null())
                .unwrap_or(false);
            if stored_done || completed {
                return Err(format!("wave target '{target_id}' is terminal"));
            }
            let notes = obj
                .entry("progress_notes".to_string())
                .or_insert_with(|| Value::Array(vec![]));
            if !notes.is_array() {
                *notes = Value::Array(vec![]);
            }
            notes
                .as_array_mut()
                .expect("just made an array")
                .push(note.clone());
        }
        // The same plan-rung map _run_op rides: locked_mutate's ladder write
        // is skipped when no map is supplied, and the wave append must land
        // the derived status (a live wave node reads back status: idea). The
        // map rides MutateInput, so locked_mutate recomputes and takes the
        // curation pre-image from the derived status (no touched_at stamp).
        let mut plan_rungs = std::collections::BTreeMap::new();
        for e in entries.iter() {
            let Some(id) = e.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(plan_path) = e.get("plan_path").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(plan_path) {
                if let Some(status) = plan_frontmatter_status(&text) {
                    plan_rungs.insert(id.to_string(), status);
                }
            }
        }
        crate::graph_store::locked_mutate(
            &graph,
            crate::graph_store::MutateInput {
                entries,
                canonical_path: None,
                base_version,
                plan_rungs: Some(plan_rungs),
            },
            crate::graph_store::DEFAULT_LOCK_TIMEOUT,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    };
    let char_count = text.chars().count();
    run().map_err(|e| refused(format!("Error: {e}"), 2))?;
    // The receipt prints only after a read-back confirms the note landed:
    // an op that reports success without persisting is a refusal, never a
    // folded-as-wave receipt (the Python leg's _confirm_note_landed).
    let readback = crate::graph_store::read_rows(&graph).ok().and_then(|rows| {
        rows.iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(target_id))
            .cloned()
    });
    let landed = readback
        .as_ref()
        .and_then(|row| row.get("progress_notes"))
        .and_then(Value::as_array)
        .map(|notes| notes.iter().any(|n| n.get("ts") == note.get("ts")))
        .unwrap_or(false);
    if readback.is_none() {
        return Err(refused(
            "Error: wave append reported success but the read-back could not confirm it \
             (store read failed); verify the target before retrying",
            2,
        ));
    }
    if !landed {
        return Err(refused(
            format!(
                "Error: wave append reported success but the note is not in the published \
                 target ('{target_id}' read back without it); the write did not land"
            ),
            2,
        ));
    }
    if json_out {
        println!(
            "{}",
            super::render::py_json_pretty(&json!({
                "outcome": "wave",
                "node_id": target_id,
                "note": note,
                "minted_id": Value::Null,
            }))
        );
    } else {
        println!("wave note appended to {target_id} progress_notes ({char_count} chars); no node minted. Read it: fno backlog get {target_id}");
    }
    Ok(())
}

// -- idea: the pre-mint fold gate (A8) and wave filing (A9) --

fn stdin_is_interactive() -> bool {
    // SAFETY: isatty only reads the descriptor's mode, like law_match.rs.
    unsafe { libc::isatty(0) == 1 }
}

/// discovery._PATH_RE: a slash-bearing path token not preceded by an
/// identifier character. The regex crate has no lookbehind, so the scan is
/// hand-rolled over the same ASCII classes, leftmost-longest, non-overlapping.
fn path_spans(text: &str) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let is_class = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-';
    let mut out = Vec::new();
    let n = b.len();
    let mut start = 0;
    while start < n {
        if !is_class(b[start])
            || (start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_'))
        {
            start += 1;
            continue;
        }
        let mut pos = start;
        let mut segments = 0usize;
        loop {
            let seg_start = pos;
            while pos < n && is_class(b[pos]) {
                pos += 1;
            }
            if pos == seg_start || pos >= n || b[pos] != b'/' {
                pos = seg_start;
                break;
            }
            pos += 1;
            segments += 1;
        }
        if segments == 0 {
            start += 1;
            continue;
        }
        let tail_start = pos;
        while pos < n && is_class(b[pos]) {
            pos += 1;
        }
        if pos == tail_start {
            start += 1;
            continue;
        }
        out.push((start, pos));
        start = pos;
    }
    out
}

/// Drop fenced code blocks and quoted spans (a quoted path is something the
/// filer read, not a surface they touch), then lift the path tokens.
pub(crate) fn filing_paths(text: &str) -> Vec<String> {
    let mut cleaned = text.to_string();
    // ```.*?``` (dotall): collapse each fence pair.
    while let Some(open) = cleaned.find("```") {
        let Some(close_rel) = cleaned[open + 3..].find("```") else {
            cleaned.replace_range(open.., " ");
            break;
        };
        let close = open + 3 + close_rel + 3;
        cleaned.replace_range(open..close, " ");
    }
    // (?<![A-Za-z0-9])(['\"])[^'\"\n]*\1(?![A-Za-z0-9]): one span at a
    // time, same quote both ends, no quote or newline inside.
    loop {
        let bytes = cleaned.as_bytes();
        let mut hit: Option<(usize, usize)> = None;
        for q in 0..bytes.len() {
            let quote = bytes[q];
            if quote != b'\'' && quote != b'"' {
                continue;
            }
            if q > 0 && (bytes[q - 1].is_ascii_alphanumeric()) {
                continue;
            }
            let Some(rel) = bytes[q + 1..]
                .iter()
                .position(|&c| c == quote || c == b'\n')
            else {
                continue;
            };
            if rel == 0 {
                continue; // empty span: * matches at least one char
            }
            let end_quote = q + 1 + rel;
            if bytes[end_quote] != quote {
                continue; // hit a newline first
            }
            match bytes.get(end_quote + 1) {
                Some(c) if c.is_ascii_alphanumeric() => continue,
                _ => {}
            }
            hit = Some((q, end_quote + 1));
            break;
        }
        let Some((s, e)) = hit else { break };
        cleaned.replace_range(s..e, " ");
    }
    path_spans(&cleaned)
        .into_iter()
        .map(|(s, e)| cleaned[s..e].to_string())
        .collect()
}

fn strip_path_cell(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Trailing parenthetical annotation: `path (template)` -> `path`.
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') && !s[open + 1..s.len() - 1].contains('(') {
            let head = s[..open].trim_end().to_string();
            s = head;
        }
    }
    s = s.replace('`', "").trim().to_string();
    // Trailing line suffix: `path.py:42` or `path.py:42-90` -> `path.py`.
    let byte_len = s.len();
    let mut cut = byte_len;
    let b = s.as_bytes();
    // scan back over digits, then an optional -digits, then the colon
    let mut i = b.len();
    while i > 0 && b[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i < b.len() && i > 0 && b[i - 1] == b'-' {
        let mut j = i - 1;
        while j > 0 && b[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < i - 1 && j > 0 && b[j - 1] == b':' {
            cut = j - 1;
        }
    } else if i < b.len() && i > 0 && b[i - 1] == b':' {
        cut = i - 1;
    }
    if cut < byte_len {
        s.truncate(cut);
    }
    s
}

fn is_separator_row(line: &str) -> bool {
    // ^\|[\s:|-]+\|[\s:|-]*$
    let b = line.as_bytes();
    if b.first() != Some(&b'|') {
        return false;
    }
    let mut i = 1;
    let mut seen = 0;
    while i < b.len() && b[i] != b'|' {
        if !(b[i].is_ascii_whitespace() || b[i] == b':' || b[i] == b'|' || b[i] == b'-') {
            return false;
        }
        seen += 1;
        i += 1;
    }
    if seen == 0 || i >= b.len() {
        return false;
    }
    i += 1; // the second '|'
    while i < b.len() {
        if !(b[i].is_ascii_whitespace() || b[i] == b':' || b[i] == b'|' || b[i] == b'-') {
            return false;
        }
        i += 1;
    }
    true
}

fn looks_like_separator_prefix(line: &str) -> bool {
    // ^\|[\s:|-]+\|
    let b = line.as_bytes();
    if b.first() != Some(&b'|') {
        return false;
    }
    let mut i = 1;
    let mut seen = 0;
    while i < b.len() && b[i] != b'|' {
        if !(b[i].is_ascii_whitespace() || b[i] == b':' || b[i] == b'|' || b[i] == b'-') {
            return false;
        }
        seen += 1;
        i += 1;
    }
    seen > 0 && i < b.len() && b[i] == b'|'
}

const FILE_HEADINGS: [&str; 5] = [
    "files to modify",
    "files to change",
    "files touched",
    "file ownership map",
    "files",
];

fn extract_files_from_section(lines: &[&str], heading_index: usize) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut saw_header = false;
    let mut saw_separator = false;
    for line in lines.iter().skip(heading_index + 1) {
        let stripped = line.trim();
        if stripped.starts_with("##") {
            break;
        }
        if !stripped.starts_with('|') || is_separator_row(stripped) {
            if saw_header && saw_separator && stripped.is_empty() {
                saw_header = false;
                saw_separator = false;
            }
            continue;
        }
        if !saw_header {
            saw_header = true;
            continue;
        }
        if !saw_separator {
            if looks_like_separator_prefix(stripped) {
                saw_separator = true;
                continue;
            }
            saw_separator = true; // tolerate a missing separator; fall through
        }
        let body = stripped.trim_matches('|');
        let Some(cell) = body.split('|').next() else {
            continue;
        };
        let cell = strip_path_cell(cell);
        if !cell.is_empty() && !files.contains(&cell) {
            files.push(cell);
        }
    }
    files
}

fn scan_one_plan(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("Warning: collision parser cannot read {path:?}");
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut found: Vec<String> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        if !stripped.starts_with("##") {
            continue;
        }
        let heading = stripped.trim_start_matches('#').trim().to_lowercase();
        if FILE_HEADINGS.iter().any(|h| heading.starts_with(h)) {
            for f in extract_files_from_section(&lines, idx) {
                if !found.contains(&f) {
                    found.push(f);
                }
            }
        }
    }
    found
}

/// collision.parse_files_to_modify: the plan's declared file surface. A
/// folder plan spreads its tables across 00-INDEX.md and the phase files.
fn parse_files_to_modify(plan_path: &Path) -> Vec<String> {
    if plan_path.is_dir() {
        let mut found: Vec<String> = Vec::new();
        let mut children: Vec<PathBuf> = std::fs::read_dir(plan_path)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        children.sort();
        for child in children {
            for f in scan_one_plan(&child) {
                if !found.contains(&f) {
                    found.push(f);
                }
            }
        }
        return found;
    }
    if !plan_path.exists() {
        return Vec::new();
    }
    scan_one_plan(plan_path)
}

/// discovery.candidates over the filing pool: union the FTS lane with
/// relatedness recall, ranked by relatedness score. `limit=5` here, matching
/// the Python fold gate.
fn discovery_candidates(
    title: &str,
    details: &str,
    pool: &[Value],
    graph: &Path,
    limit: usize,
) -> (Vec<(String, f64, String, Vec<String>)>, Option<String>) {
    let mut by_id: std::collections::BTreeMap<String, &Value> = std::collections::BTreeMap::new();
    for e in pool {
        if let Some(id) = e.get("id").and_then(Value::as_str) {
            by_id.insert(id.to_string(), e);
        }
    }
    let query = [title, details]
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut fts_ids: Vec<String> = Vec::new();
    let mut warning: Option<String> = None;
    if query.is_empty() {
        warning = Some(
            "graph entries were injected without the graph file; \
             the fts lane is unavailable"
                .to_string(),
        );
    } else {
        match super::search::search(&super::api::Store::new(graph), &query, None) {
            Ok(ids) => {
                for id in ids {
                    if by_id.contains_key(&id) && !fts_ids.contains(&id) {
                        fts_ids.push(id);
                    }
                }
            }
            // Golden captures ran with the FTS cache present, so the exact
            // degraded wording is unpinned; the shape mirrors Python's str(exc).
            Err(err) => warning = Some(err.0),
        }
    }
    let mut incoming = Map::new();
    incoming.insert("id".into(), json!("__incoming__"));
    incoming.insert("title".into(), json!(title));
    incoming.insert("details".into(), json!(details));
    incoming.insert("domain".into(), json!("code"));
    let incoming = Value::Object(incoming);
    let related_rows = relatedness::similar_nodes(&incoming, pool, usize::MAX, None);
    let mut all: Vec<(String, f64, String, Vec<String>)> = Vec::new();
    for (id, score, reason) in related_rows {
        if !by_id.contains_key(&id) {
            continue;
        }
        all.push((id, score, reason, vec!["relatedness".into()]));
    }
    for id in &fts_ids {
        if let Some(entry) = by_id.get(id) {
            if all.iter().any(|(eid, _, _, _)| eid == id) {
                let slot = all
                    .iter_mut()
                    .find(|(eid, _, _, _)| eid == id)
                    .expect("checked above");
                slot.3.push("fts".into());
            } else {
                let (score, reason) = relatedness::score_pair(&incoming, entry, false);
                all.push((id.clone(), score, reason, vec!["fts".into()]));
            }
        }
    }
    all.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    all.truncate(limit);
    (all, warning)
}

/// The fold gate's candidate list: live filing siblings, then any live plan
/// surface naming the same files. Raw stored statuses; the write pipeline
/// keeps them in step.
fn fold_candidates(title: &str, details: Option<&str>, entries: &[Value]) -> (Vec<Value>, String) {
    let sidecar = super::settings::graph_path()
        .parent()
        .map(|p| p.join("relatedness.json"))
        .unwrap_or_else(|| PathBuf::from("relatedness.json"));
    let (candidates, source) = relatedness::filing_candidates(entries, &sidecar);
    let details_owned = details.unwrap_or("").to_string();
    let graph = super::settings::graph_path();
    let (ranked, warning) = discovery_candidates(title, &details_owned, &candidates, &graph, 5);
    let mut out: Vec<Value> = Vec::new();
    for (node_id, score, reason, lanes) in &ranked {
        let Some(node) = candidates
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(node_id.as_str()))
        else {
            continue;
        };
        let mut reason = reason.clone();
        if lanes.len() == 1 && lanes[0] == "fts" {
            reason = format!(
                "fts-only: {}",
                if reason.is_empty() {
                    "full-text match"
                } else {
                    reason.as_str()
                }
            );
        }
        // Python serializes the lane set sorted.
        let mut lanes = lanes.clone();
        lanes.sort();
        out.push(json!({
            "id": node_id,
            "title": node.get("title").cloned().unwrap_or(Value::Null),
            "status": node.get("status").cloned().unwrap_or(Value::Null),
            "holder": live_worker(node_id),
            "score": score,
            "evidence": reason,
            "lanes": lanes,
        }));
    }
    let mut source = source;
    if let (true, Some(warning)) = (!out.is_empty() || !ranked.is_empty(), warning.as_ref()) {
        if !ranked.is_empty() {
            source = format!("{source}; degraded: {warning}");
        }
    }
    // A live plan surface is an independent fold signal when the filing
    // names one of the same files.
    let incoming: std::collections::BTreeSet<String> =
        filing_paths(&details_owned).into_iter().collect();
    if !incoming.is_empty() {
        let known: std::collections::BTreeSet<String> = out
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        for node in entries {
            let Some(surface_node_id) = node.get("id").and_then(Value::as_str) else {
                continue;
            };
            if known.contains(surface_node_id)
                || node.get("status").and_then(Value::as_str) != Some("in_progress")
            {
                continue;
            }
            let Some(plan_path) = node.get("plan_path").and_then(Value::as_str) else {
                continue;
            };
            let declared: std::collections::BTreeSet<String> =
                parse_files_to_modify(Path::new(plan_path))
                    .into_iter()
                    .collect();
            let overlap: Vec<String> = incoming.intersection(&declared).cloned().collect();
            if !overlap.is_empty() {
                out.push(json!({
                    "id": surface_node_id,
                    "title": node.get("title").cloned().unwrap_or(Value::Null),
                    "status": node.get("status").cloned().unwrap_or(Value::Null),
                    "holder": live_worker(surface_node_id),
                    "score": 1.0,
                    "evidence": format!("file overlap: {}", overlap.join(", ")),
                }));
            }
        }
    }
    (out, source)
}

/// typer.confirm's y/N read, reduced: prints the prompt with typer's default
/// marker, reads one line, yes when it starts y/Y. EOF answers no.
fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    use std::io::BufRead;
    let line = std::io::stdin().lock().lines().next();
    matches!(line, Some(Ok(text)) if {
        let t = text.trim();
        t.starts_with('y') || t.starts_with('Y')
    })
}

/// `fno backlog idea` — the plan-less capture verb with the pre-mint fold
/// gate and wave filing. Share add's create path once the gate passes.
pub fn run_idea(tail: &[String]) -> i32 {
    let idea = match parse_idea(tail) {
        ParsedIdea::Help => {
            print!("{}", include_str!("idea_help.txt"));
            return 0;
        }
        ParsedIdea::Refusal { message, exit } => {
            eprintln!("{message}");
            return exit;
        }
        ParsedIdea::Args(a) => a,
    };
    match idea_create(&idea) {
        Ok(()) => 0,
        Err(r) => {
            eprintln!("{}", r.message);
            r.exit
        }
    }
}

fn wave_normalize_difficulty(raw: &str) -> Result<String, Refusal> {
    let band = raw.trim().to_lowercase();
    if !["low", "medium", "high"].contains(&band.as_str()) {
        // The wave path carries no DIFFICULTY_HELP suffix; the golden pins it.
        return Err(refused(
            format!("Error: invalid difficulty '{raw}'; must be one of: low, medium, high"),
            2,
        ));
    }
    Ok(band)
}

fn idea_create(idea: &IdeaArgs) -> Result<(), Refusal> {
    refuse_tracker_owned("idea")?;
    let add = &idea.add;
    let details = read_text_arg(&add.details, &idea.details_file)?;
    let wave_body: Option<String> = details.clone().or_else(|| add.description.clone());

    if let Some(wave_of) = &idea.wave_of {
        if add.evidence.is_some() {
            return Err(refused("Error: --wave-of cannot record a creation vote", 2));
        }
        if idea.separate {
            return Err(refused(
                "Error: --wave-of and --separate are mutually exclusive",
                2,
            ));
        }
        let mut topology_flags: Vec<String> = Vec::new();
        let mut push_flag = |given: bool, name: &str| {
            if given {
                topology_flags.push(name.to_string());
            }
        };
        push_flag(add.blocked_by.is_some(), "--blocked-by");
        push_flag(add.parent.is_some(), "--parent");
        push_flag(add.roadmap_id.is_some(), "--roadmap-id");
        push_flag(add.vision_path.is_some(), "--vision-path");
        push_flag(add.project.is_some(), "--project");
        push_flag(add.cwd.is_some(), "--cwd");
        push_flag(add.size.is_some(), "--size");
        push_flag(add.batch.is_some(), "--batch");
        push_flag(!add.related.is_empty(), "--related");
        push_flag(add.type_ != "feature", "--type");
        push_flag(add.priority != "p2", "--priority");
        if !topology_flags.is_empty() {
            return Err(refused(
                format!(
                    "Error: wave filing cannot use node-only topology flags: {}",
                    topology_flags.join(", ")
                ),
                2,
            ));
        }
        let difficulty = match &add.difficulty {
            None => {
                if !stdin_is_interactive() {
                    return Err(refused(
                        "Error: non-interactive wave filing requires --difficulty (low, medium, high)",
                        2,
                    ));
                }
                let mut value;
                loop {
                    eprint!("Difficulty (low|medium|high): ");
                    use std::io::BufRead;
                    let line = std::io::stdin()
                        .lock()
                        .lines()
                        .next()
                        .unwrap_or_else(|| Ok(String::new()))
                        .unwrap_or_default();
                    value = line.trim().to_lowercase();
                    if ["low", "medium", "high"].contains(&value.as_str()) {
                        break;
                    }
                }
                value
            }
            Some(d) => wave_normalize_difficulty(d)?,
        };
        let graph = super::settings::graph_path();
        let entries =
            crate::graph_store::read_rows(&graph).map_err(|e| refused(format!("{e}"), 2))?;
        let target_id = node_ref::resolve_asserted_id(wave_of, &entries, "--wave-of", None)
            .map_err(|(message, exit)| refused(message, exit))?;
        file_wave(
            idea.json_out,
            &target_id,
            &add.title,
            wave_body.as_deref(),
            &difficulty,
            add.source_node.as_deref(),
        )?;
        return Ok(());
    }

    if add.difficulty.is_some() && !idea.separate {
        if let Ok(normalized) =
            wave_normalize_difficulty(add.difficulty.as_deref().expect("checked"))
        {
            let graph = super::settings::graph_path();
            let entries =
                crate::graph_store::read_rows(&graph).map_err(|e| refused(format!("{e}"), 2))?;
            let (candidates, candidate_source) =
                fold_candidates(&add.title, wave_body.as_deref(), &entries);
            if !candidates.is_empty() {
                if add.evidence.is_some() {
                    return Err(refused(
                        "Error: --evidence cannot be used when an idea fold is offered; \
                         rerun with --separate to mint a node and record the creation vote.",
                        2,
                    ));
                }
                let top = &candidates[0];
                let top_id = top.get("id").and_then(Value::as_str).unwrap_or("");
                let wave_command = format!(
                    "fno backlog idea {} --wave-of {} --difficulty {normalized}",
                    shlex_quote(&add.title),
                    top_id,
                );
                let marker = format!(
                    "fold offered: {} status={} holder={} evidence={}",
                    top.get("title").and_then(Value::as_str).unwrap_or(""),
                    top.get("status").and_then(Value::as_str).unwrap_or(""),
                    top.get("holder").and_then(Value::as_str).unwrap_or("none"),
                    top.get("evidence").and_then(Value::as_str).unwrap_or(""),
                );
                let separate_command = format!(
                    "fno backlog idea {} --separate --difficulty {normalized}",
                    shlex_quote(&add.title),
                );
                if !stdin_is_interactive() {
                    if idea.json_out {
                        println!(
                            "{}",
                            super::render::py_json_pretty(&json!({
                                "outcome": "choice_required",
                                "marker": marker,
                                "candidate_source": candidate_source,
                                "candidates": candidates,
                                "wave_command": wave_command,
                                "separate_command": separate_command,
                                "minted_id": Value::Null,
                            }))
                        );
                    } else {
                        println!("{marker}");
                        println!("wave: {wave_command}");
                        println!("separate: {separate_command}");
                    }
                    return Ok(());
                }
                if confirm(&format!("{marker}. Fold into {top_id}?")) {
                    file_wave(
                        idea.json_out,
                        top_id,
                        &add.title,
                        wave_body.as_deref(),
                        &normalized,
                        add.source_node.as_deref(),
                    )?;
                    return Ok(());
                }
            }
        }
    }

    let mut mint = add.clone();
    mint.details = details;
    let born = create_node(&mint, &|_| None)?;
    println!(
        "{}",
        super::render::py_json_pretty(&json!({"id": born.id, "title": mint.title}))
    );
    Ok(())
}
