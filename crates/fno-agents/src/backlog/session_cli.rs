//! The blueprint session lifecycle door: add, open, close, backfill, reap-open.
//!
//! Ported from the deleted Python `graph/_session.py`. The receipts here are
//! pinned by `cli/tests/goldens/test_session_golden.py`; a receipt may only
//! change when that golden changes with it. The lease shape: a 2h TTL,
//! `pid_unavailable` when no durable session pid
//! resolves (a transient pid would read stale at once), and the session id.

use std::cell::RefCell;
use std::path::PathBuf;

use serde_json::{json, Value};

use super::node_ref::resolve_tiers;
use super::settings;

pub(crate) const BLUEPRINT_HOLDER_PREFIX: &str = "blueprint-session:";
const HANDOVER_HOLDER_PREFIX: &str = "spawn-handover:";
const BLUEPRINT_TTL_MS: i64 = 2 * 60 * 60 * 1000;
const SESSION_PHASES: &[&str] = &["think", "blueprint", "execute", "review", "ship"];

pub fn run(tail: &[String]) -> i32 {
    let sub = tail.first().map(String::as_str).unwrap_or("");
    if sub != "-h" && sub != "--help" && !sub.is_empty() {
        // The external-backend guard the Python surface carried: session
        // verbs own graph state, so under any other tracker backend they
        // refuse before any read or write.
        let backend = std::env::var("FNO_TRACKER_BACKEND")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "graph".to_string());
        if backend != "graph" {
            eprintln!(
                "fno backlog session {sub}: this verb owns graph state; under \
the {backend} tracker backend it is refused. Track the item in the tracker \
by its id."
            );
            return 1;
        }
    }
    match tail.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            println!("{}", include_str!("session_help.txt").trim_end());
            0
        }
        Some("add") => run_add(&tail[1..]),
        Some("open") => run_open(&tail[1..]),
        Some("close") => run_close(&tail[1..]),
        Some("backfill") => run_backfill(&tail[1..]),
        Some("reap-open") => run_reap_open(&tail[1..]),
        Some(other) => {
            eprintln!("fno backlog session: unknown subcommand '{other}'");
            2
        }
    }
}

fn run_backfill(args: &[String]) -> i32 {
    // The Python leg was always a delegate: the native backfill walk already
    // lived in `session_backfill.rs`, reached behind a `--graph` flag.
    let mut argv: Vec<String> = vec![
        "--graph".into(),
        settings::graph_path().display().to_string(),
    ];
    argv.extend(args.iter().cloned());
    super::super::session_backfill::run(&argv)
}

/// The identity a session verb acts as: the explicit `--harness`/`--session-id`
/// overrides, else the ambient walk. Either half empty refuses (the caller
/// prints its own verb-specific line).
fn eff_identity(harness: Option<&str>, session_id: Option<&str>) -> Option<(String, String)> {
    let (ambient_session, ambient_harness) = crate::claims::resolve_identity();
    let h = harness
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or(ambient_harness)
        .unwrap_or_default();
    let s = session_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or(ambient_session)
        .unwrap_or_default();
    if h.is_empty() || s.is_empty() {
        None
    } else {
        Some((h, s))
    }
}

fn resolve_exact(rows: &[Value], token: &str) -> Option<String> {
    resolve_tiers(rows, token)?
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn read_graph(graph: &PathBuf) -> Result<Vec<Value>, String> {
    crate::graph_store::read_rows(graph).map_err(|e| e.to_string())
}

fn now_utc_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `types.normalize_phase`: the retired `do` spelling maps to `execute`.
fn normalize_phase(phase: &str) -> String {
    if phase == "do" {
        "execute".to_string()
    } else {
        phase.to_string()
    }
}

/// The open (phase, session) row an `--ended-at` append would close, keyed on
/// the session id. None when no such open row exists.
fn open_row_to_end<'a>(
    rows: &'a [Value],
    node_id: &str,
    phase: &str,
    session: &str,
) -> Option<&'a Value> {
    let mut open = None;
    for entry in rows {
        if entry.get("id").and_then(Value::as_str) != Some(node_id) {
            continue;
        }
        for row in entry
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if row.get("phase").and_then(Value::as_str) == Some(phase)
                && row.get("session_id").and_then(Value::as_str) == Some(session)
                && crate::graph_store::is_open_phase_row(row, phase)
            {
                open = Some(row);
            }
        }
    }
    open
}

/// What the row's session ACTUALLY answered as, read from its own transcript
/// (the port of `store._observe_model` -> `provenance.observed`). Only claude
/// and codex keep a per-session file; a prefix-shaped id can never be proven
/// to be this session, so it reads `unreadable`. Never fails a stamp.
fn observe_model(harness: &str, session_id: &str) -> Value {
    match harness {
        "claude" | "codex" => {}
        _ => return json!({"kind": "not-file-backed"}),
    }
    if session_id.chars().count() < 32 {
        return json!({
            "kind": "unreadable",
            "reason": format!("session id {session_id:?} is prefix-shaped; \
        a glob match cannot be proven to be this session"),
        });
    }
    let path = match harness {
        "claude" => {
            let projects = crate::claude_drive::claude_projects_dir();
            super::super::claude_transcript_paths::resolve_transcript(&projects, session_id)
        }
        _ => crate::codex_store::codex_rollout_path(None, session_id),
    };
    let Some(path) = path else {
        return json!({"kind": "no-transcript"});
    };
    read_transcript_model(harness, &path)
}

/// The bounded tail read (256 KiB) with the per-harness model reader; a torn
/// final line stays `unreadable` and corrects itself on the next stamp.
fn read_transcript_model(harness: &str, path: &PathBuf) -> Value {
    const TAIL: u64 = 256 * 1024;
    let Ok(meta) = std::fs::metadata(path) else {
        return json!({"kind": "no-transcript"});
    };
    let size = meta.len();
    let windowed = size > TAIL;
    let Ok(blob) = std::fs::read(path) else {
        return json!({"kind": "unreadable", "reason": "read failed"});
    };
    let start = if windowed { (size - TAIL) as usize } else { 0 };
    let text = String::from_utf8_lossy(&blob[start..]).into_owned();
    if text.is_empty() {
        return json!({"kind": "no-model-yet"});
    }
    if !text.ends_with('\n') {
        return json!({"kind": "unreadable", "reason": "torn final line (mid-write)"});
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if windowed {
        lines.remove(0);
    }
    let mut last: Option<String> = None;
    let mut samples = 0usize;
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let model = match harness {
            "claude" => {
                if rec.get("type").and_then(Value::as_str) != Some("assistant") {
                    None
                } else {
                    rec.get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(Value::as_str)
                        .filter(|m| *m != "<synthetic>")
                        .map(str::to_string)
                }
            }
            _ => {
                if rec.get("type").and_then(Value::as_str) != Some("turn_context") {
                    None
                } else {
                    rec.get("payload")
                        .and_then(|p| p.get("model"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
            }
        };
        if let Some(model) = model {
            if !model.is_empty() {
                last = Some(model);
                samples += 1;
            }
        }
    }
    match last {
        None => json!({"kind": "no-model-yet"}),
        Some(model) => json!({"kind": "observed", "model": model, "samples": samples}),
    }
}

fn run_open(args: &[String]) -> i32 {
    let mut node: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--harness" => harness = it.next().cloned(),
            "--session-id" => session_id = it.next().cloned(),
            "--json" | "-J" => json_out = true,
            "-h" | "--help" => {
                println!("Usage: fno backlog session open [OPTIONS] NODE");
                return 0;
            }
            other if other.starts_with('-') => {
                eprintln!("fno backlog session open: unknown flag {other}");
                return 2;
            }
            other => node = Some(other.to_string()),
        }
    }
    let Some(node) = node else {
        eprintln!(
            "Usage: fno backlog session open [OPTIONS] NODE\n\nError: a node argument is required."
        );
        return 2;
    };
    let Some((h, s)) = eff_identity(harness.as_deref(), session_id.as_deref()) else {
        eprintln!("session open: no ambient identity for {node}; run inside a session.");
        return 2;
    };
    let graph = settings::graph_path();
    let rows = match read_graph(&graph) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("session open: could not read the graph cleanly: {e}");
            return 3;
        }
    };
    let Some(node_id) = resolve_exact(&rows, &node) else {
        eprintln!("session open: no exact node matches '{node}'.");
        return 2;
    };
    let claim_key = format!("node:{node_id}");
    let holder = format!("{BLUEPRINT_HOLDER_PREFIX}{s}");
    let root = crate::claims::claims_root_for(&claim_key);
    let (state, rec) = crate::claims::status(&claim_key, root.as_deref());
    if !matches!(state, crate::claims::ClaimState::Free)
        && rec.as_ref().map(|r| r.holder == holder).unwrap_or(false)
    {
        eprintln!("session open: node:{node_id} is already open for this session ({holder}).");
        return 1;
    }
    if let Some(own) = own_handover_holder(&s) {
        let own_held = rec.as_ref().map(|r| r.holder == own).unwrap_or(false);
        if own_held
            && matches!(
                state,
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
            )
        {
            // fno agents spawn claimed the node for this worker: plan under it.
            if json_out {
                println!(
                    "{}",
                    json!({"node_id": node_id, "status": "joined", "claim_key": claim_key, "holder": own})
                );
            } else {
                println!("joined {node_id} holder={own}");
            }
            return 0;
        }
    }
    let pid = crate::claims::open_session_pid();
    let opts = crate::claims::AcquireOpts {
        pid: pid.map(|p| p as u32),
        pid_unavailable: pid.is_none(),
        ttl_ms: Some(BLUEPRINT_TTL_MS),
        reason: Some(format!("blueprint session for {node_id}")),
        identity: Some((s.clone(), h.clone())),
        root: root.clone(),
        ..Default::default()
    };
    match crate::claims::acquire(&claim_key, &holder, opts) {
        crate::claims::AcquireOutcome::Acquired(claim) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "node_id": node_id, "status": "opened", "claim_key": claim_key,
                        "holder": holder, "harness": h, "session_id": s,
                        "acquired_at": claim.acquired_at,
                    })
                );
            } else {
                println!("opened {node_id} holder={holder}");
            }
            0
        }
        crate::claims::AcquireOutcome::HeldByOther {
            holder: other, pid, ..
        } => {
            eprintln!(
                "session open: node:{node_id} held by {other} (pid={}); no planner started.",
                pid.map(|p| p.to_string()).unwrap_or_else(|| "None".into())
            );
            1
        }
        crate::claims::AcquireOutcome::Error(e) => {
            eprintln!("session open: node:{node_id} could not be claimed: ClaimError: {e}.");
            3
        }
    }
}

/// The spawn-handover holder fno agents spawn took for this session, or None:
/// the env export first, then the registry row naming this exact session id.
fn own_handover_holder(session_id: &str) -> Option<String> {
    if let Ok(env) = std::env::var("FNO_NODE_CLAIM_HOLDER") {
        let env = env.trim();
        if env.starts_with(HANDOVER_HOLDER_PREFIX) {
            return Some(env.to_string());
        }
    }
    roster_name_for_session(session_id).map(|name| format!("{HANDOVER_HOLDER_PREFIX}{name}"))
}

/// The roster name bound to this harness session id, or None. Two DIFFERENT
/// names answering one session id is ambiguous and reads None (the caller
/// keeps its previous resolution unchanged).
fn roster_name_for_session(session_id: &str) -> Option<String> {
    let home = crate::paths::AgentsHome::from_env_opt()?;
    let registry = crate::state::load_registry_with_counts(&home.registry_json())
        .ok()?
        .0;
    let mut found: Option<String> = None;
    for entry in &registry.entries {
        if entry.harness_session_id.as_deref() == Some(session_id) && !entry.name.is_empty() {
            if let Some(prior) = &found {
                if prior != &entry.name {
                    return None;
                }
            } else {
                found = Some(entry.name.clone());
            }
        }
    }
    found
}

fn release_into(
    receipt: &RefCell<Value>,
    claim_key: &str,
    holder: &str,
    root: Option<&std::path::Path>,
) {
    match crate::claims::release(claim_key, holder, root, None) {
        Ok(()) => {
            receipt.borrow_mut()["claim_released"] = Value::Bool(true);
            receipt.borrow_mut()["claim_holder"] = Value::String(holder.to_string());
        }
        Err(e) => {
            receipt.borrow_mut()["claim_released"] = Value::Bool(false);
            eprintln!(
                "session close: {claim_key} not released: ClaimError: {e}. \
It stays held until its TTL expires."
            );
        }
    }
}

fn run_close(args: &[String]) -> i32 {
    let mut node: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut launch: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--summary" => summary = it.next().cloned(),
            "--launch" => launch = it.next().cloned(),
            "--harness" => harness = it.next().cloned(),
            "--session-id" => session_id = it.next().cloned(),
            "--started-at" => started_at = it.next().cloned(),
            "--json" | "-J" => json_out = true,
            "-h" | "--help" => {
                println!(
                    "Usage: fno backlog session close [OPTIONS] NODE --summary TEXT --launch CMD"
                );
                return 0;
            }
            other if other.starts_with('-') => {
                eprintln!("fno backlog session close: unknown flag {other}");
                return 2;
            }
            other => node = Some(other.to_string()),
        }
    }
    let Some(node) = node else {
        eprintln!("Usage: fno backlog session close [OPTIONS] NODE\n\nError: a node argument is required.");
        return 2;
    };
    let Some(summary) = summary else {
        eprintln!(
            "Usage: fno backlog session close [OPTIONS] NODE\n\nError: Missing option '--summary'."
        );
        return 2;
    };
    let Some(launch) = launch else {
        eprintln!(
            "Usage: fno backlog session close [OPTIONS] NODE\n\nError: Missing option '--launch'."
        );
        return 2;
    };
    let summary = summary.trim().to_string();
    let launch = launch.trim().to_string();
    if summary.is_empty() || launch.is_empty() {
        eprintln!("session close: summary and launch must be non-empty.");
        return 2;
    }
    let Some((h, s)) = eff_identity(harness.as_deref(), session_id.as_deref()) else {
        eprintln!(
            "session close: no ambient identity for {node}; \
pass --harness/--session-id or run inside a session."
        );
        return 2;
    };
    let graph = settings::graph_path();
    let rows = match read_graph(&graph) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("session close: could not read the graph cleanly: {e}");
            return 3;
        }
    };
    let Some(node_id) = resolve_exact(&rows, &node) else {
        eprintln!("session close: no exact node matches '{node}'.");
        return 2;
    };
    let claim_key = format!("node:{node_id}");
    let root = crate::claims::claims_root_for(&claim_key);
    let (state, claim) = crate::claims::status(&claim_key, root.as_deref());
    let claim_holder = claim.as_ref().map(|r| r.holder.clone()).unwrap_or_default();
    let blueprint_holder = format!("{BLUEPRINT_HOLDER_PREFIX}{s}");
    let free = matches!(state, crate::claims::ClaimState::Free);
    let blueprint_held = !free && claim_holder == blueprint_holder;
    let mut started_at = started_at;
    if started_at.is_none() {
        let own = blueprint_held
            || (!free
                && own_handover_holder(&s)
                    .map(|holder| holder == claim_holder)
                    .unwrap_or(false));
        let acquired = if own {
            claim.as_ref().map(|r| r.acquired_at)
        } else {
            None
        };
        if let Some(ms) = acquired {
            started_at = Some(
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_default(),
            );
        }
    }
    let ended_at = now_utc_stamp();
    let row = match crate::graph_keeper::session_row(
        "blueprint",
        &h,
        &s,
        None,
        started_at.as_deref(),
        Some(&ended_at),
        Some(observe_model(&h, &s)),
        None,
    ) {
        Ok(row) => row,
        Err(e) => {
            eprintln!("session close: {}", e);
            return 2;
        }
    };
    let found_cell = RefCell::new(false);
    let ok = crate::backlog::mutate_single_row(&graph, "session_append", |rows| {
        let (found, _added) = crate::graph_keeper::session_append(rows, &node_id, row.clone())
            .map_err(|e| e.to_string())?;
        found_cell.replace(found);
        Ok(found)
    });
    if let Err(e) = ok {
        eprintln!("session close: {e}");
        return 2;
    }
    if !found_cell.into_inner() {
        eprintln!("session close: node {node_id} disappeared before close.");
        return 2;
    }
    let receipt = RefCell::new(json!({
        "node_id": node_id, "status": "closed", "phase": "blueprint",
        "harness": h, "session_id": s, "summary": summary, "launch": launch,
        "ended_at": ended_at, "claim_released": false,
    }));
    let launch_verb = launch.split_whitespace().next().unwrap_or("");
    let parsed = crate::provider::parse_verb_token(launch_verb);
    if parsed
        .as_ref()
        .map(|(_, namespaced)| *namespaced)
        .unwrap_or(false)
    {
        let stored_verb = format!("/fno:{}", parsed.unwrap().0);
        let verb_row = stored_verb.clone();
        let _ = crate::backlog::mutate_single_row(&graph, "session_close_verb", |rows| {
            for entry in rows.iter_mut() {
                if entry.get("id").and_then(Value::as_str) == Some(node_id.as_str())
                    && entry.get("dispatch_verb").and_then(Value::as_str) != Some(verb_row.as_str())
                {
                    entry
                        .as_object_mut()
                        .expect("row is an object")
                        .insert("dispatch_verb".into(), Value::String(verb_row.clone()));
                    break;
                }
            }
            Ok(true)
        });
    } else {
        eprintln!(
            "session close: dispatch_verb not written: launch token \
'{launch_verb}' is not a plugin-qualified verb."
        );
    }
    let env_holder = std::env::var("FNO_NODE_CLAIM_HOLDER")
        .unwrap_or_default()
        .trim()
        .to_string();
    if env_holder.starts_with(HANDOVER_HOLDER_PREFIX) {
        release_into(&receipt, &claim_key, &env_holder, root.as_deref());
    } else if blueprint_held {
        release_into(&receipt, &claim_key, &blueprint_holder, root.as_deref());
    } else if let Some(handover) = own_handover_holder(&s) {
        if handover == claim_holder {
            release_into(&receipt, &claim_key, &handover, root.as_deref());
        }
    }
    if json_out {
        println!("{}", receipt.into_inner());
    } else {
        println!("blueprint closed {node_id} ({h}:{s})");
        println!("summary: {summary}");
        println!("launch: {launch}");
    }
    0
}

/// Node ids carrying `pr_number`, optionally narrowed to one repo slug
/// (pr_number is not unique across repos; the url carries the slug). The
/// matching is the keeper's own: primary or additional_prs by number, the
/// repo compared against the `/pull/<n>` url tail.
fn find_nodes_for_pr(rows: &[Value], pr: i64, repo: Option<&str>) -> Vec<String> {
    rows.iter()
        .filter(|e| crate::graph_store::is_dict(e))
        .filter(|e| crate::graph_keeper::node_carries_pr(e, pr, repo))
        .filter_map(|e| crate::graph_store::entry_id(e).map(str::to_string))
        .collect()
}
/// Best-effort `owner/repo` for this checkout: git origin, parsed. None on
/// every failure; the caller degrades to unscoped resolution.
pub(crate) fn resolve_current_repo_slug() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let trimmed = url.strip_suffix(".git").unwrap_or(&url);
    let owner_repo: Vec<&str> = if let Some((owner, repo)) = trimmed.rsplit_once(':') {
        // scp spelling git@github.com:owner/repo
        let segs: Vec<&str> = owner.rsplit('/').collect();
        vec![segs[0], repo]
    } else {
        trimmed
            .rsplit('/')
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    if owner_repo.len() != 2 {
        return None;
    }
    Some(format!("{}/{}", owner_repo[0], owner_repo[1]))
}

fn skip_receipt(
    who: &str,
    phase: &str,
    reason: &str,
    node_id: Option<&str>,
    h: &str,
    s: &str,
    json_out: bool,
) -> i32 {
    eprintln!("session add: {reason} (target={who} phase={phase}). Skipped.");
    if json_out {
        println!(
            "{}",
            json!({
                "node_id": node_id, "status": "skipped", "reason": reason,
                "phase": phase, "harness": h, "session_id": s, "added": false,
            })
        );
    }
    0
}

fn refuse_foreign_row(
    rows: &[Value],
    node_id: &str,
    phase: &str,
    eff_session: &str,
    ambient: Option<&str>,
    h: &str,
) -> Result<(), i32> {
    let prior = open_row_to_end(rows, node_id, phase, eff_session);
    let Some(prior) = prior else {
        return Ok(());
    };
    if Some(eff_session) == ambient {
        return Ok(());
    }
    let owner_harness = prior.get("harness").and_then(Value::as_str).unwrap_or(h);
    eprintln!(
        "session add: {owner_harness}:{eff_session} owns an open {phase} row \
on {node_id}; only that session ends it here. A dead session's row \
closes with: fno backlog session reap-open {node_id} --phase {phase} \
--harness {owner_harness} --session-id {eff_session} \
(after proving death)."
    );
    Err(2)
}

#[allow(clippy::too_many_arguments)]
fn run_add(args: &[String]) -> i32 {
    let mut node: Option<String> = None;
    let mut phase: Option<String> = None;
    let mut pr: Option<i64> = None;
    let mut repo: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut ended_at: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut require_session: Option<String> = None;
    let mut guard_plan: Option<String> = None;
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--phase" => phase = it.next().cloned(),
            "--pr-number" => match it.next().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) => pr = Some(v),
                None => return 2,
            },
            "--repo" => repo = it.next().cloned(),
            "--harness" => harness = it.next().cloned(),
            "--session-id" => session_id = it.next().cloned(),
            "--effort" => effort = it.next().cloned(),
            "--ended-at" | "--at" => ended_at = it.next().cloned(),
            "--started-at" | "--claimed-at" => started_at = it.next().cloned(),
            "--require-session" => require_session = it.next().cloned(),
            "--guard-plan" => guard_plan = it.next().cloned(),
            "--json" | "-J" => json_out = true,
            "-h" | "--help" => {
                println!("Usage: fno backlog session add [OPTIONS] [NODE]");
                return 0;
            }
            other if other.starts_with('-') => {
                eprintln!("fno backlog session add: unknown flag {other}");
                return 2;
            }
            other => node = Some(other.to_string()),
        }
    }
    let Some(phase) = phase else {
        eprintln!(
            "Usage: fno backlog session add [OPTIONS] [NODE]\n\nError: Missing option '--phase'."
        );
        return 2;
    };
    let phase = normalize_phase(&phase);
    if node.is_none() == pr.is_none() {
        eprintln!("session add: pass exactly one of NODE or --pr-number.");
        return 2;
    }
    if guard_plan.is_some() && pr.is_some() {
        eprintln!("session add: --guard-plan requires NODE, not --pr-number.");
        return 2;
    }
    let who = node
        .clone()
        .unwrap_or_else(|| format!("pr#{}", pr.unwrap_or(0)));
    let Some((h, s)) = eff_identity(harness.as_deref(), session_id.as_deref()) else {
        eprintln!(
            "session add: no ambient identity for {who} phase={phase}; \
pass --harness/--session-id or run inside a session. Skipped."
        );
        return 2;
    };
    let (ambient_session, _) = crate::claims::resolve_identity();
    if let Some(require) = &require_session {
        if session_id.is_some() || harness.is_some() {
            eprintln!(
                "session add: --require-session cannot be combined with \
--session-id/--harness (it would verify one identity and record another)."
            );
            return 2;
        }
        let ambient = ambient_session.clone().unwrap_or_default();
        if ambient != require.trim() {
            return skip_receipt(
                &who,
                &phase,
                &format!(
                    "ambient session {ambient:?} != required {:?}",
                    require.trim()
                ),
                None,
                &h,
                &s,
                json_out,
            );
        }
    }
    let mut repo = repo;
    if pr.is_some() && repo.is_none() {
        repo = resolve_current_repo_slug();
        if repo.is_none() {
            eprintln!(
                "session add: could not resolve this checkout's repo slug for pr#{}; \
matching on the bare PR number (skips on cross-repo ambiguity).",
                pr.unwrap_or(0)
            );
        }
    }
    let graph = settings::graph_path();
    let rows = match read_graph(&graph) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("session add: could not read the graph cleanly: {e}");
            return 3;
        }
    };
    let prior_open = if ended_at.is_some() {
        find_open_row_for_stamp(&rows, node.as_deref(), pr, repo.as_deref(), &phase, &s)
    } else {
        None
    };
    let (node_id, added) = if let Some(pr) = pr {
        let ids = find_nodes_for_pr(&rows, pr, repo.as_deref());
        if ids.len() != 1 {
            let status = if ids.is_empty() {
                "no-node"
            } else {
                "ambiguous"
            };
            let detail = if ids.is_empty() {
                String::new()
            } else {
                format!(" (candidates: {})", ids.join(", "))
            };
            let repair = if ids.is_empty() {
                format!(
                    " A node whose PR was never stamped is invisible here. \
Link it with `fno backlog update <node-id> --pr-number {pr}`, or pass the \
node id directly: `fno backlog session add <node-id> --phase {phase}`."
                )
            } else {
                String::new()
            };
            eprintln!(
                "session add: PR {pr} maps to {status}{detail} (phase={phase}); \
resolution is exact and never fans out.{repair} Skipped."
            );
            if json_out {
                println!(
                    "{}",
                    json!({
                        "node_id": Value::Null, "status": status, "phase": phase,
                        "harness": h, "session_id": s, "added": false,
                        "candidates": ids,
                    })
                );
            }
            return 0;
        }
        stamp_row(
            &graph,
            &phase,
            &h,
            &s,
            effort.as_deref(),
            ended_at.as_deref(),
            started_at.as_deref(),
            &ids[0],
        )
    } else {
        let node = node.clone().unwrap_or_default();
        let Some(resolved) = resolve_exact(&rows, &node) else {
            eprintln!("session add: no node matches '{node}' (phase={phase}).");
            return 2;
        };
        if let Some(plan) = &guard_plan {
            match plan_claims(plan) {
                None => eprintln!(
                    "session add: plan {plan} is unreadable or declares no \
claims; agreement not evaluated for {resolved}."
                ),
                Some(claims) if !claims.contains(&resolved) => {
                    return skip_receipt(
                        &who,
                        &phase,
                        &format!("plan {plan} claims {claims:?} != node {resolved}"),
                        Some(&resolved),
                        &h,
                        &s,
                        json_out,
                    );
                }
                Some(_) => {}
            }
        }
        if ended_at.is_some() {
            if let Err(code) =
                refuse_foreign_row(&rows, &resolved, &phase, &s, ambient_session.as_deref(), &h)
            {
                return code;
            }
        }
        stamp_row(
            &graph,
            &phase,
            &h,
            &s,
            effort.as_deref(),
            ended_at.as_deref(),
            started_at.as_deref(),
            &resolved,
        )
    };
    let (node_id, added) = match (node_id, added) {
        (Some(id), added) => (id, added),
        (None, _) => {
            eprintln!("session add: node not found (phase={phase}).");
            return 2;
        }
    };
    let ended_existing = prior_open
        .map(|p| p.get("harness").and_then(Value::as_str) == Some(h.as_str()))
        .unwrap_or(false)
        && ended_at.is_some();
    if json_out {
        println!(
            "{}",
            json!({
                "node_id": node_id,
                "status": if ended_existing { "ended" } else if added { "added" } else { "duplicate" },
                "phase": phase, "harness": h, "session_id": s, "added": added,
            })
        );
    } else if ended_existing {
        println!("ended {phase} {h}:{s} on {node_id}");
    } else {
        let state = if added {
            "recorded"
        } else {
            "already recorded"
        };
        println!("{state} {phase} {h}:{s} on {node_id}");
    }
    0
}

fn find_open_row_for_stamp<'a>(
    rows: &'a [Value],
    node: Option<&str>,
    pr: Option<i64>,
    repo: Option<&str>,
    phase: &str,
    s: &str,
) -> Option<&'a Value> {
    let node_id = match (node, pr) {
        (Some(n), _) => resolve_exact(rows, n),
        (None, Some(pr)) => {
            let ids = find_nodes_for_pr(rows, pr, repo);
            if ids.len() == 1 {
                Some(ids[0].clone())
            } else {
                None
            }
        }
        _ => None,
    }?;
    open_row_to_end(rows, &node_id, phase, s)
}

/// Build the validated row and append it; returns (node_id, added). `None`
/// node_id reads not-found (no mutation).
fn stamp_row(
    graph: &PathBuf,
    phase: &str,
    h: &str,
    s: &str,
    effort: Option<&str>,
    ended_at: Option<&str>,
    started_at: Option<&str>,
    node_id: &str,
) -> (Option<String>, bool) {
    let row = match crate::graph_keeper::session_row(
        phase,
        h,
        s,
        effort,
        started_at,
        ended_at,
        Some(observe_model(h, s)),
        None,
    ) {
        Ok(row) => row,
        Err(e) => {
            eprintln!("session add: {} (target={node_id} phase={phase})", e);
            return (None, false);
        }
    };
    let result: RefCell<(bool, bool)> = RefCell::new((false, false));
    let ok = crate::backlog::mutate_single_row(graph, "session_append", |rows| {
        let (found, added) = crate::graph_keeper::session_append(rows, node_id, row.clone())
            .map_err(|e| e.to_string())?;
        result.replace((found, added));
        Ok(found)
    });
    if let Err(e) = ok {
        eprintln!("session add: {e}");
        return (None, false);
    }
    let (found, added) = result.into_inner();
    if found {
        (Some(node_id.to_string()), added)
    } else {
        (None, false)
    }
}

/// The plan's frontmatter `claims:` names, or None when unreadable/absent
/// (agreement-unknown, which skips loudly rather than failing the stamp).
fn plan_claims(plan_path: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(plan_path).ok()?;
    let mut in_frontmatter = false;
    let mut claims: Option<Vec<String>> = None;
    let mut node: Option<String> = None;
    for line in text.lines() {
        if line.trim() == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        if let Some(rest) = line.strip_prefix("claims:") {
            let rest = rest.trim();
            if !rest.is_empty() {
                let cleaned = rest.trim_start_matches('[').trim_end_matches(']');
                claims = Some(
                    cleaned
                        .split(',')
                        .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
                        .filter(|p| !p.is_empty())
                        .collect(),
                );
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("node:") {
            let rest = rest.trim();
            if !rest.is_empty() {
                node = Some(rest.to_string());
            }
        }
    }
    let mut ids = claims.unwrap_or_default();
    if let Some(n) = node {
        if !ids.contains(&n) {
            ids.push(n);
        }
    }
    if ids.is_empty() {
        return None;
    }
    Some(ids)
}

fn run_reap_open(args: &[String]) -> i32 {
    let mut node: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut phase = "execute".to_string();
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--harness" => harness = it.next().cloned(),
            "--session-id" => session_id = it.next().cloned(),
            "--phase" => phase = it.next().cloned().unwrap_or_default(),
            "--json" | "-J" => json_out = true,
            "-h" | "--help" => {
                println!("Usage: fno backlog session reap-open [NODE] --harness NAME --session-id ID [--phase PHASE]");
                return 0;
            }
            other if other.starts_with('-') => {
                eprintln!("fno backlog session reap-open: unknown flag {other}");
                return 2;
            }
            other => node = Some(other.to_string()),
        }
    }
    let Some(harness) = harness else {
        eprintln!(
            "Usage: fno backlog session reap-open [NODE]\n\nError: Missing option '--harness'."
        );
        return 2;
    };
    let Some(session_id) = session_id else {
        eprintln!(
            "Usage: fno backlog session reap-open [NODE]\n\nError: Missing option '--session-id'."
        );
        return 2;
    };
    let phase = normalize_phase(&phase);
    let graph = settings::graph_path();
    let resolved = match &node {
        None => None,
        Some(node) => {
            let rows = match read_graph(&graph) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("session reap-open: could not read the graph cleanly: {e}");
                    return 3;
                }
            };
            match resolve_exact(&rows, node) {
                Some(id) => Some(id),
                None => {
                    eprintln!("session reap-open: no exact node matches '{node}'.");
                    return 2;
                }
            }
        }
    };
    let report: RefCell<Option<Value>> = RefCell::new(None);
    let op = crate::backlog::mutate_single_row(&graph, "session_reap_open", |rows| {
        let receipt = crate::graph_keeper::session_reap_open(
            rows,
            resolved.as_deref(),
            &phase,
            harness.trim(),
            session_id.trim(),
            None,
        )
        .map_err(|e| e.to_string())?;
        report.replace(Some(receipt));
        Ok(true)
    });
    if let Err(e) = op {
        eprintln!("session reap-open: {e}");
        return 2;
    }
    let Some(receipt) = report.into_inner() else {
        eprintln!("session reap-open: the store answered nothing.");
        return 2;
    };
    let found = receipt
        .get("found")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let row_removed = receipt
        .get("row_removed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let row_closed = receipt
        .get("row_closed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if resolved.is_none() {
        // The death-cascade form: every node holding an open row settles.
        if !found {
            eprintln!("session reap-open: no open row carries that identity on any node.");
            return 1;
        }
        if json_out {
            println!("{}", receipt);
        } else {
            let nodes = receipt
                .get("node_ids")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!("settled {nodes}: row_removed={row_removed} row_closed={row_closed}");
        }
        return 0;
    }
    let node_id = resolved.unwrap();
    // Read back and verify the settlement, exactly like the Python verb.
    let rows = match read_graph(&graph) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("session reap-open: node {node_id} disappeared on read-back: {e}");
            return 1;
        }
    };
    let Some(entry) = rows
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(node_id.as_str()))
    else {
        eprintln!("session reap-open: node {node_id} disappeared on read-back.");
        return 1;
    };
    let node_rows: Vec<&Value> = entry
        .get("sessions")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let want_phases: Vec<&str> = if phase == "all" {
        SESSION_PHASES.to_vec()
    } else {
        vec![phase.as_str()]
    };
    let matching_open = node_rows.iter().any(|row| {
        want_phases
            .iter()
            .any(|ph| crate::graph_store::is_open_phase_row(row, ph))
            && row.get("harness").and_then(Value::as_str) == Some(harness.trim())
            && row.get("session_id").and_then(Value::as_str) == Some(session_id.trim())
    });
    let (higher_precedence, expected_in_progress, remaining) =
        crate::graph_keeper::reap_settlement_state(entry);
    let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
    let status_ok = higher_precedence || (status == "in_progress") == expected_in_progress;
    if matching_open || !status_ok {
        eprintln!(
            "session reap-open: read-back did not settle {node_id} \
(matching_open={matching_open}, status={status:?}, remaining_open_do={remaining})."
        );
        return 1;
    }
    if json_out {
        let mut out = receipt.clone();
        let obj = out.as_object_mut().expect("receipt is an object");
        obj.insert("node_id".into(), Value::String(node_id.clone()));
        obj.insert("settled".into(), Value::Bool(true));
        obj.insert(
            "status_after".into(),
            entry.get("status").cloned().unwrap_or(Value::Null),
        );
        obj.insert("remaining_open_do".into(), Value::Number(remaining.into()));
        println!("{out}");
    } else {
        println!(
            "settled {node_id}: row_removed={row_removed} row_closed={row_closed} \
status={status} remaining_open_do={remaining}"
        );
    }
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_pr_filter_keeps_only_the_named_repo() {
        let rows = vec![
            serde_json::json!({"id": "x-aaaa1111", "pr_number": 7,
                "pr_url": "https://github.com/o/r1/pull/7"}),
            serde_json::json!({"id": "x-bbbb2222", "pr_number": 7}),
            serde_json::json!({"id": "x-cccc3333", "additional_prs":
                [{"number": 7, "url": "https://github.com/o/r2/pull/7"}]}),
        ];
        assert_eq!(
            super::find_nodes_for_pr(&rows, 7, Some("o/r1")),
            vec!["x-aaaa1111"]
        );
        assert_eq!(super::find_nodes_for_pr(&rows, 7, None).len(), 3);
        assert!(super::find_nodes_for_pr(&rows, 8, None).is_empty());
    }

    #[test]
    fn plan_claims_reads_node_key_as_a_single_claim() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan-node.md");
        std::fs::write(&plan, "---\nstatus: ready\nnode: x-aaaa\n---\n\n# plan\n").unwrap();
        assert_eq!(
            super::plan_claims(plan.to_str().unwrap()),
            Some(vec!["x-aaaa".to_string()])
        );
    }

    #[test]
    fn plan_claims_unions_claims_and_node() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("both.md");
        std::fs::write(
            &plan,
            "---\nstatus: ready\nnode: x-1111\nclaims: [x-2222, x-3333]\n---\n\n# plan\n",
        )
        .unwrap();
        assert_eq!(
            super::plan_claims(plan.to_str().unwrap()),
            Some(vec![
                "x-2222".to_string(),
                "x-3333".to_string(),
                "x-1111".to_string()
            ])
        );
    }
}
