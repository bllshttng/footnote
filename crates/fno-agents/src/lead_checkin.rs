//! `lead-checkin`: one verb runs the lead check-in body.
//!
//! Gathers the readings the lead skill names, prints them in a fixed order,
//! diffs against the previous canonical `lead_checkin` row, and emits that
//! row from the same values it printed. It reads, prints, diffs and
//! journals; it never decides (no spawn, no reap, no lever, no graph write).
//! Contract: docs/architecture/lead.md and skills/lead/SKILL.md.
//!
//! Python resolves the paths Python owns (journals, graph, handoffs, FAQs)
//! and relays here, the same split `lead-history` applies; the caller's
//! team scope, level and board state are resolved NATIVELY when `--scope`
//! is not passed (the scope fold's `resolve_scope`); the gather and
//! the row write are native so the Python-tree ratchet holds. The scope fold
//! is the `org-fold` fold in process, the previous row comes through the
//! `lead-history` scan, and the board is the `board` payload read in
//! process, so the check-in cannot disagree with the surfaces a lead already
//! reads.
//!
//! `lead-checkin [--scope SCOPE] --events-path PATH [--events-path ...]
//!              --graph PATH --handoffs-dir PATH [--faqs-dir PATH]
//!              [--board-state PATH] [--emit-path PATH] [--change TEXT]
//!              [--no-emit] [--json]`
//!
//! rc 0 a completed beat, 3 when an asked-for row was not journalled or
//! stdout could not be written, 2 usage failure.
use crate::lead_history::LEAD_CHECKIN;
use crate::org_board::{read_board, BoardOpts};
use crate::org_fold::org_fold;
use crate::scrape::fno_bin;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[path = "lead_checkin_watch_projection.rs"]
mod watch_projection;

/// The numeric keys this verb owns and diffs versus the previous beat.
const NUMERIC_DIFF_KEYS: [&str; 11] = [
    "open_prs",
    "free_claim_no_driver",
    "blocked",
    "escalations_open",
    "escalations_overdue",
    "owned_active",
    "live_workers",
    "undelivered",
    "held_open",
    "blueprint_running",
    "blueprint_ceiling",
];

/// Diff keys absent from the previous beat's data: a hand-journaled baseline
/// lacking any of them must read unmeasured, never "no change".
fn missing_diff_keys(prev_data: Option<&Value>) -> Vec<&'static str> {
    match prev_data {
        Some(prev) => NUMERIC_DIFF_KEYS
            .iter()
            .copied()
            .filter(|key| prev.get(*key).is_none())
            .collect(),
        None => NUMERIC_DIFF_KEYS.to_vec(),
    }
}

/// Render cap for the per-node rows a org line prints (the count in the
/// payload stays whole, only the rendered rows are cut, as the board does).
pub(crate) const MAX_ORG_ROWS: usize = 25;

pub(crate) fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn iso_now() -> String {
    let dt: chrono::DateTime<chrono::Utc> = SystemTime::now().into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// readers

/// One reading could not be taken; the beat continues without it.
struct ReaderError(String);

pub(crate) struct Reading {
    pub(crate) name: &'static str,
    pub(crate) ok: bool,
    pub(crate) value: Value,
    pub(crate) error: String,
}

impl Reading {
    pub(crate) fn failed(name: &'static str, error: String) -> Self {
        Reading {
            name,
            ok: false,
            value: Value::Null,
            error,
        }
    }
    pub(crate) fn took(name: &'static str, value: Value) -> Self {
        Reading {
            name,
            ok: true,
            value,
            error: String::new(),
        }
    }
}

/// Paths and knobs the relay resolves Python-side; the readers read them.
struct Ctx {
    scope: String,
    /// The team scope's level (epic or project), resolved Python-side from
    /// the registry row that holds the team; the fold refuses a levelless
    /// team, because compile_forced branches on it.
    level: Option<i64>,
    events_paths: Vec<PathBuf>,
    graph: PathBuf,
    cwd: PathBuf,
    handoffs_dir: PathBuf,
    faqs_dir: Option<PathBuf>,
    board_state: Option<PathBuf>,
    emit_path: Option<PathBuf>,
    emit: bool,
}

fn run_capture(argv: &[std::ffi::OsString]) -> Result<(i32, String, String), String> {
    let (prog, rest) = argv.split_first().ok_or("empty argv")?;
    let out = Command::new(prog)
        .args(rest)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", prog.to_string_lossy()))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Keep the last useful stderr line, skipping config warnings that can hide it.
pub(crate) fn stderr_cause(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find(|line| !line.starts_with("fno config:"))
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("no stderr");
    let end = line
        .char_indices()
        .nth(120)
        .map(|(index, _)| index)
        .unwrap_or(line.len());
    line[..end].to_string()
}

pub(crate) fn gh_error_cause(error: &str) -> String {
    stderr_cause(
        error
            .split_once(" failed: ")
            .map_or(error, |(_, stderr)| stderr),
    )
}

pub(crate) fn fno_verb(args: &[&str]) -> Result<(i32, String, String), String> {
    let mut argv = vec![fno_bin()];
    argv.extend(args.iter().map(std::ffi::OsString::from));
    run_capture(&argv)
}

/// The team-keyed handoff doc for one scope, newest existing file first.
/// The key scheme matches the precompact writer (`config paths handoff
/// --scope`), so the two cannot drift; no doc yet is a failed reading, not a
/// placeholder beat. Takes the directory and scope rather than `Ctx` so the
/// stop gate's stale-doc resolver calls the same one.
pub(crate) fn team_handoff_doc(handoffs_dir: &Path, scope: &str) -> Result<PathBuf, String> {
    let key = format!("team-{}", sanitize_scope_key(scope));
    if key == "team-" {
        return Err("empty scope names no canon doc".into());
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let it = std::fs::read_dir(handoffs_dir)
        .map_err(|_| format!("no canon handoff doc for scope {scope}"))?;
    for entry in it.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(&format!("-{key}.md")) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| format!("no canon handoff doc for scope {scope}"))
}

pub(crate) fn sanitize_scope_key(scope: &str) -> String {
    let mut out = String::new();
    for ch in scope.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// The raw text between `<!-- fno:user -->` and its close, marker lines
/// excluded. A missing closing marker is CONTENT, never a parse error: the
/// capture runs to the next marker fence, one of the writer's own section
/// headings, or end of file. Mirrors scripts/lib/canon-doc-marker.sh.
fn extract_user_marker(text: &str) -> Option<String> {
    const WRITER_HEADINGS: [&str; 5] = [
        "## Merge order and why (",
        "## Open decisions awaiting the operator (",
        "## Gaps and open thinking (",
        "## Workarounds in force (",
        "## User notes (",
    ];
    let mut grabbed = String::new();
    let mut inside = false;
    for line in text.lines() {
        if !inside {
            if line.contains("<!-- fno:user -->") {
                inside = true;
            }
            continue;
        }
        if line.contains("<!-- /fno:user -->") {
            return Some(grabbed);
        }
        let fence = line.trim_end();
        if fence.starts_with("<!-- fno:")
            && fence.ends_with(" -->")
            && fence[9..fence.len() - 4].len() > 0
            && fence[9..fence.len() - 4]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Some(grabbed);
        }
        if WRITER_HEADINGS.iter().any(|h| line.starts_with(h)) {
            return Some(grabbed);
        }
        grabbed.push_str(line);
        grabbed.push('\n');
    }
    if inside {
        Some(grabbed)
    } else {
        None
    }
}

/// True when the captured user-block text is only the seed placeholder.
fn is_user_placeholder(text: &str) -> bool {
    const PLACEHOLDER: &str =
        "_(write here; the machine reads this every refresh and never edits it)_";
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    !text.is_empty() && strip(text) == strip(PLACEHOLDER)
}

fn r_user_notes(ctx: &Ctx) -> Result<Value, String> {
    let doc = team_handoff_doc(&ctx.handoffs_dir, &ctx.scope)?;
    let text =
        std::fs::read_to_string(&doc).map_err(|e| format!("{}: unreadable: {e}", doc.display()))?;
    let block = extract_user_marker(&text)
        .ok_or_else(|| "user block marker missing or doc unreadable".to_string())?;
    if is_user_placeholder(&block) {
        return Ok(Value::Null);
    }
    Ok(Value::String(block))
}

pub(crate) fn board_queue<'a>(board: &'a Value, name: &str) -> Result<&'a Value, String> {
    for q in board
        .get("queues")
        .and_then(|q| q.as_array())
        .into_iter()
        .flatten()
    {
        if q.get("name").and_then(|n| n.as_str()) == Some(name) {
            let status = q.get("status").and_then(|s| s.as_str()).unwrap_or("ok");
            if status != "ok" {
                let error = s_str(q, "error").unwrap_or("");
                return Err(format!("{name} queue {status}: {error}"));
            }
            return Ok(q);
        }
    }
    Err(format!("board payload names no {name} queue"))
}

fn open_pr_count() -> Result<i64, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("open PR listing failed: {e}"))?;
    let pages = crate::pr_push::gh_api_pages(
        "gh",
        &cwd,
        "repos/{owner}/{repo}/pulls?state=open&per_page=100",
    )
    .map_err(|error| format!("open PR listing failed: {}", gh_error_cause(&error)))?;
    Ok(open_pr_total(&pages))
}

fn open_pr_total(pages: &[Value]) -> i64 {
    pages
        .iter()
        .map(|page| page.as_array().map(|rows| rows.len() as i64).unwrap_or(0))
        .sum()
}

/// The check-in board's own budget, above the 30s hand-run default. A
/// fleet-sized board prices its truth batch alone at `20s + 750ms` a holder
/// (a 24-handle page self-bounds at 38s), and the beat has no interactive
/// caller waiting on it: measured 2026-09-28, three beats in a row starved
/// the probe on the hand default and lost undriven_pr, unplanned and
/// blocked_child to "batch of 33-34 handles timed out". A blind beat cannot
/// seat work; 45s buys the full feed at fleet size.
const BOARD_BUDGET_MS: u64 = 45_000;

fn fetch_board(ctx: &Ctx) -> Result<Value, String> {
    let opts = BoardOpts {
        state_path: ctx.board_state.clone(),
        cwd: Some(ctx.cwd.clone()),
        budget_ms: BOARD_BUDGET_MS,
        ..Default::default()
    };
    Ok(read_board(&opts))
}

fn fetch_fold(ctx: &Ctx) -> Result<Value, String> {
    let teams = vec![json!({"scope": ctx.scope, "level": ctx.level})];
    let payload = org_fold(
        &ctx.graph,
        &ctx.cwd,
        None,
        &crate::paths::AgentsHome::from_env().registry_json(),
        &teams,
    )
    .map_err(|e| format!("scope fold unreadable: {e}"))?;
    let mine = payload
        .get("scope_nodes")
        .and_then(|s| s.get(&ctx.scope))
        .cloned()
        .unwrap_or(Value::Null);
    if mine.get("status").and_then(|s| s.as_str()) != Some("ok") {
        let reason = s_str(&mine, "reason").unwrap_or("the fold did not run");
        return Err(format!("scope fold unreadable: {reason}"));
    }
    Ok(json!({
        "fold": mine,
        "stuck": payload.get("stuck").cloned().unwrap_or(Value::Null),
        "owned_scopes": payload.get("owned_scopes").cloned().unwrap_or(Value::Null),
    }))
}

fn r_board(
    board: &Result<Value, String>,
    org: &Result<Value, String>,
    open_prs: Result<i64, String>,
) -> Result<Value, String> {
    let board = board.clone()?;
    let org = org.clone()?;
    let mut blocked_on: Vec<String> = Vec::new();
    let mut blocked = 0i64;
    if let Some(rows) = org
        .get("stuck")
        .and_then(|s| s.get("blocked"))
        .and_then(|b| b.as_array())
    {
        blocked = rows.len() as i64;
        for row in rows {
            let id = s_str(row, "id").unwrap_or("?");
            let deps: Vec<String> = row
                .get("blocked_by")
                .and_then(|b| b.as_array())
                .map(|a| {
                    a.iter()
                        .map(|v| v.as_str().unwrap_or(&v.to_string()).to_string())
                        .collect()
                })
                .unwrap_or_default();
            blocked_on.push(format!("{id} on {}", deps.join(",")));
        }
    }
    let undriven = board_queue(&board, "undriven_pr")?;
    Ok(json!({
        "open_prs": open_prs?,
        "free_claim_no_driver": undriven.get("count").and_then(|c| c.as_i64()).unwrap_or(0),
        "blocked": blocked,
        "blocked_on": blocked_on,
    }))
}

/// Open escalation notes in the lead's scope (or scopeless), with the default
/// each overdue call takes. Unreadable is a reading that says so, never a
/// zero: a blind spot must not read as a quiet board.
fn r_escalations(cwd: &Path, folded: &Result<Value, String>) -> Result<Value, String> {
    let dir = crate::escalation::dir(cwd);
    let notes = crate::escalation::scan(&dir).map_err(|e| format!("unreadable ({e})"))?;
    let folded = folded
        .clone()
        .map_err(|e| format!("board fold unreadable, so scope filtering is down ({e})"))?;
    let scope_ids: std::collections::HashSet<&str> = folded
        .get("fold")
        .and_then(|f| f.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|n| n.get("id").and_then(|v| v.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let now: chrono::DateTime<chrono::Utc> = SystemTime::now().into();
    let mut rows = Vec::new();
    let mut open = 0i64;
    let mut overdue = 0i64;
    for note in notes {
        if note.status != "open" {
            continue;
        }
        if let Some(node) = &note.node {
            if !scope_ids.contains(node.as_str()) {
                continue;
            }
        }
        open += 1;
        let state = if crate::escalation::overdue(&note, now) {
            overdue += 1;
            match (
                note.class.as_str(),
                note.on_silence.as_str(),
                note.recommend,
            ) {
                ("irreversible", _, _) => "overdue: waits (irreversible)".to_string(),
                (_, "take-recommended", Some(n)) if n >= 1 => {
                    format!("overdue: take option {n} and record it")
                }
                _ => "overdue: waits (on_silence wait)".to_string(),
            }
        } else {
            "open".to_string()
        };
        rows.push(json!({
            "title": note.title,
            "class": note.class,
            "deadline": note.deadline,
            "state": state,
        }));
    }
    Ok(json!({"open": open, "overdue": overdue, "rows": rows}))
}

fn r_blocked_child(board: &Result<Value, String>) -> Result<Value, String> {
    let board = board.clone()?;
    let rows = board_queue(&board, "blocked_child")?
        .get("rows")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "node": row.get("id"),
                    "session": row.get("session"),
                    "age_minutes": row.get("age_minutes"),
                    "reason": row.get("reason"),
                })
            })
            .collect(),
    ))
}

fn r_org(folded: &Result<Value, String>) -> Result<Value, String> {
    let org = folded.clone()?;
    let fold = &org["fold"];
    let sum_active = |key: &str| -> i64 {
        crate::org_fold::ACTIVE_STATUSES
            .iter()
            .filter_map(|s| {
                fold.get(key)
                    .and_then(|c| c.get(*s))
                    .and_then(|v| v.as_i64())
            })
            .sum()
    };
    let active_nodes = sum_active("counts");
    // An unread owned count is null with its reason, never a quiet zero.
    let owned_active = if fold.get("owned_counts").map(Value::is_null).unwrap_or(true) {
        Value::Null
    } else {
        json!(sum_active("owned_counts"))
    };
    let mut rows: Vec<Value> = Vec::new();
    for n in fold
        .get("nodes")
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
    {
        // Another team's node stays off this lead's list, but an unread
        // owner mark keeps the row: a broken instrument never hides work.
        if n.get("owned") == Some(&json!(false)) {
            continue;
        }
        let session = n
            .get("sessions")
            .and_then(|s| s.as_array())
            .and_then(|s| s.first())
            .cloned()
            .unwrap_or(Value::Null);
        rows.push(json!({
            "id": n.get("id"),
            "status": n.get("status"),
            "worker": n.get("worker"),
            "pr_number": n.get("pr_number"),
            "session": session,
        }));
    }
    Ok(json!({
        "active_nodes": active_nodes,
        "owned_active": owned_active,
        "owned_reason": fold.get("owned_reason").cloned().unwrap_or(Value::Null),
        "total_nodes": fold.get("total").and_then(|t| t.as_i64()).unwrap_or(0),
        "rows": rows,
        "epics": fold.get("epics").cloned().unwrap_or(Value::Null),
        "epic_cap": fold.get("epic_cap").cloned().unwrap_or(Value::Null),
    }))
}

/// The `epics:` line under the org reading: each scope epic that holds an
/// open child, fullest first, as `open/cap` cells, ` full` on the rows whose
/// next open child the write cap refuses. `-` and a trailing `(cap unset)`
/// say no cap is configured.
fn epic_line(org: &Value) -> String {
    let Some(rows) = org.get("epics").and_then(|v| v.as_array()) else {
        return "epics: unmeasured".into();
    };
    if rows.is_empty() {
        return "epics: none in scope holds an open child".into();
    }
    let cap_raw = org.get("epic_cap").and_then(Value::as_u64);
    let cap = cap_raw.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
    let cells: Vec<String> = rows
        .iter()
        .map(|row| {
            let id = s_str(row, "id").unwrap_or("?");
            let n = row
                .get("open_children")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // `full` is a cap word: with no cap configured the producer
            // never emits it, and a stray one must not read as a refusal.
            let full =
                if cap_raw.is_some() && row.get("full").and_then(Value::as_bool) == Some(true) {
                    " full"
                } else {
                    ""
                };
            format!("{id} {n}/{cap}{full}")
        })
        .collect();
    let body = crate::org_fold::named_ids(&cells);
    if cap_raw.is_none() {
        format!("epics: {body} (cap unset)")
    } else {
        format!("epics: {body}")
    }
}

fn r_capacity() -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["doctor", "footprint", "--json"])?;
    if out.trim().is_empty() {
        return Err(format!("footprint unavailable: {}", stderr_cause(&err)));
    }
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("footprint payload did not parse: {e}"))?;
    let (_, gate_out, gate_err) = fno_verb(&["agents", "gate-status"])?;
    let gate: Value = serde_json::from_str(gate_out.trim())
        .map_err(|e| format!("gate payload did not parse: {e}: {}", gate_err.trim()))?;
    r_capacity_pair(&payload, &gate)
}

/// The pair compares footprint's CPU verdict with the gate's own cpu-share
/// row; the gate's whole verdict and the axis it refused on print beside it.
fn r_capacity_pair(footprint_payload: &Value, gate_payload: &Value) -> Result<Value, String> {
    let footprint = footprint_payload
        .pointer("/admission/verdict")
        .cloned()
        .unwrap_or(Value::Null);
    let gate_verdict = s_str(gate_payload, "verdict").unwrap_or("").to_string();
    let gate_cpu = gate_payload
        .get("rows")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("name").and_then(Value::as_str) == Some("cpu-share"))
                .and_then(|row| row.get("verdict").cloned())
        })
        .unwrap_or(Value::Null);
    let gate_axis = if gate_verdict.is_empty() || gate_verdict == "accepted" {
        Value::Null
    } else {
        s_str(gate_payload, "reason")
            .map(|r| Value::String(r.to_string()))
            .unwrap_or(Value::Null)
    };
    let fp_str = footprint.as_str().unwrap_or("").to_string();
    fn meaning(v: &str) -> Option<&str> {
        match v {
            "admit" | "pass" => Some("admit"),
            "refuse" | "undecidable" => Some("refuse"),
            "hold" => Some("hold"),
            _ => None,
        }
    }
    let lanes = gate_payload
        .get("lanes")
        .and_then(Value::as_object)
        .map(|lanes| {
            let mut rows = lanes
                .iter()
                .filter(|(provider, _)| provider.as_str() != "quota_source")
                .filter_map(|(provider, lane)| {
                    let live = lane.get("live").and_then(Value::as_u64)?;
                    let cap = lane
                        .get("cap")
                        .and_then(Value::as_u64)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".into());
                    let quota = s_str(lane, "quota").unwrap_or("unmeasured");
                    Some(format!("{provider} {live}/{cap} {quota}"))
                })
                .collect::<Vec<_>>();
            rows.sort();
            if rows.is_empty() {
                "lanes unreadable".into()
            } else {
                rows.join(", ")
            }
        })
        .unwrap_or_else(|| "lanes unreadable".into());
    let cpu_str = gate_cpu.as_str().unwrap_or("").to_string();
    let disagree = match (meaning(&fp_str), meaning(&cpu_str)) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };
    Ok(json!({
        "footprint": footprint,
        "gate": gate_verdict,
        "gate_axis": gate_axis,
        "gate_cpu": gate_cpu,
        "disagree": disagree,
        "unparsed_lines": footprint_payload
            .get("unparsed_lines")
            .and_then(|u| u.as_i64())
            .unwrap_or(0),
        "lanes": lanes,
    }))
}

fn r_team() -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["agents", "org", "--json"])?;
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("org payload did not parse: {e}: {}", err.trim()))?;
    let summary = payload.get("summary").cloned().unwrap_or(json!({}));
    let mut org_anomalies: Vec<String> = Vec::new();
    for c in payload
        .get("teams")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        let status = s_str(c, "status").unwrap_or("");
        let agree = c.get("agree").and_then(|a| a.as_bool());
        if status != "live" || agree != Some(true) {
            let holder = c.get("holder").map(|h| h.to_string()).unwrap_or_default();
            let scope = c.get("scope").map(|s| s.to_string()).unwrap_or_default();
            let reason = s_str(c, "reason")
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            org_anomalies.push(format!(
                "{holder} scope {scope} status {status} agree {agree:?}{reason}"
            ));
        }
    }

    // The split reading is its OWN registry read, not a org field: the
    // org filters to stored-live rows, so a terminal row still carrying
    // team fields is invisible there by construction, and a org failure
    // must never read as "no splits either". A failed read names itself and
    // leaves both counts null, never a measured-looking zero.
    let registry_read =
        crate::state::load_registry(&crate::paths::AgentsHome::from_env().registry_json())
            .map_err(|e| e.to_string());
    let split_read = registry_read
        .as_ref()
        .map(|registry| crate::team_split::read_team_splits(&registry.entries))
        .map_err(|e| e.to_string());
    // One dead-call reading per stale row, keyed by row name. A reading is
    // built only when the registry itself read, so an unread registry stays
    // a null-count error, never a synthesized zero. The boot reading is
    // hoisted: one sysctl per beat, not one per row.
    let boot = crate::host_boot_epoch_ms();
    let dead: std::collections::BTreeMap<String, crate::team_split::DeadCallReading> =
        match &split_read {
            Ok(splits) => splits
                .stale
                .iter()
                .map(|s| {
                    let reading = registry_read
                        .as_ref()
                        .ok()
                        .and_then(|r| r.entries.iter().find(|e| e.name == s.row))
                        .map(|e| crate::team_split::dead_call(e, boot))
                        .unwrap_or(crate::team_split::DeadCallReading::Unread(
                            "teamed row not found in the registry".to_string(),
                        ));
                    (s.row.clone(), reading)
                })
                .collect(),
            Err(_) => std::collections::BTreeMap::new(),
        };
    let (double_ruled, stale_teamed, split_read_error, ruled_lines, stale_lines) =
        team_split_fields(split_read, &dead);
    let mut anomalies = ruled_lines;
    anomalies.extend(org_anomalies);
    anomalies.extend(stale_lines);
    Ok(json!({
        "total": summary.get("total").cloned().unwrap_or(Value::Null),
        "splits": summary.get("splits").cloned().unwrap_or(Value::Null),
        "disagreements": summary.get("disagreements").cloned().unwrap_or(Value::Null),
        "double_ruled": double_ruled,
        "stale_teamed": stale_teamed,
        "split_read_error": split_read_error,
        "anomalies": anomalies,
    }))
}

/// The split half of the team reading, as JSON fields plus the anomaly
/// lines it contributes. Pure so the never-zero rule is testable without a
/// registry.
fn team_split_fields(
    read: Result<crate::team_split::TeamSplits, String>,
    dead: &std::collections::BTreeMap<String, crate::team_split::DeadCallReading>,
) -> (Value, Value, Value, Vec<String>, Vec<String>) {
    match read {
        Ok(splits) => {
            let ruled = splits
                .double_ruled
                .iter()
                .map(|s| {
                    format!(
                        "DOUBLE RULED {} held by {} live rows ({})",
                        s.scope,
                        s.holders.len(),
                        s.holders.join(", ")
                    )
                })
                .collect();
            let stale = splits
                .stale
                .iter()
                .map(|s| match dead.get(&s.row) {
                    Some(crate::team_split::DeadCallReading::Open { session_id, tool, at, boot }) => {
                        let boot_clause = match boot {
                            Some(b) => format!(", before the last boot at {b}"),
                            None => String::new(),
                        };
                        format!(
                            "stale team {} on {} (stored status {}): session {} stopped inside a {} call made at {}{}; fno agents resume {} relaunches it, fno agents rm {} drops the row and its team",
                            s.scope, s.row, s.stored_status, session_id, tool, at, boot_clause, s.row, s.row
                        )
                    }
                    Some(crate::team_split::DeadCallReading::Unread(reason)) => {
                        format!(
                            "stale team {} on {} (stored status {}); fno agents rm {} (tool-call reading: {})",
                            s.scope, s.row, s.stored_status, s.row, reason
                        )
                    }
                    _ => format!(
                        "stale team {} on {} (stored status {}); fno agents rm {}",
                        s.scope, s.row, s.stored_status, s.row
                    ),
                })
                .collect();
            (
                json!(splits.double_ruled.len() as i64),
                json!(splits.stale.len() as i64),
                Value::Null,
                ruled,
                stale,
            )
        }
        Err(reason) => (
            Value::Null,
            Value::Null,
            json!(reason),
            vec![format!("team split read failed: {reason}")],
            Vec::new(),
        ),
    }
}

/// The trailing-window refusal rate: the cheapest available proxy for
/// context degradation, no model introspection needed. Resolves its OWN
/// ambient identity (same primitive `claim_store`/`lead_verdict_inputs`
/// already use) rather than taking a flag, so no CLI surface or Python
/// wiring is needed to reach it - only claude sessions keep a per-session
/// transcript file today (`crate::claude_drive::find_transcript`), so any
/// other harness (or a claude session whose transcript cannot be found)
/// reads as an ordinary failed reading, never a silent zero.
const REFUSAL_RATE_WINDOW: usize = 200;

fn r_refusal_rate() -> Result<Value, String> {
    let transcript = own_claude_transcript()?;
    crate::refusal_rate::refusal_rate(&transcript, REFUSAL_RATE_WINDOW)
}

/// The caller's own claude transcript, shared by the check-in transcript
/// readers. Only claude sessions keep a per-session transcript file today
/// (`crate::claude_drive::find_transcript`), so any other harness (or a
/// claude session whose transcript cannot be found) reads as an ordinary
/// failed reading, never a silent zero.
pub(crate) fn own_claude_transcript() -> Result<PathBuf, String> {
    let (session_id, harness) = crate::claims::resolve_identity();
    if harness.as_deref() != Some("claude") {
        return Err("the check-in transcript readers need a claude transcript; \
             this session's harness is not claude"
            .into());
    }
    let session_id = session_id
        .ok_or_else(|| "no session id resolved from the ambient environment".to_string())?;
    crate::claude_drive::find_transcript(&session_id)
        .ok_or_else(|| format!("no transcript found for session {session_id}"))
}

/// The wake meter over the lead's own transcript. `since` is the previous
/// loop row's top-level `ts`: token spend is the per-task-id delta since
/// that beat, and with no previous row it reads the whole session.
fn r_wake_meter(since: Option<&str>) -> Result<Value, String> {
    let transcript = own_claude_transcript()?;
    let cut = match since {
        None => None,
        Some(ts) => Some(
            chrono::DateTime::parse_from_rfc3339(ts)
                .map_err(|e| format!("previous beat ts unreadable: {e}"))?
                .timestamp() as f64,
        ),
    };
    crate::wake_meter::wake_meter(&transcript, cut)
}

fn r_drain(ctx: &Ctx) -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["agents", "lead", "drain", &ctx.scope])?;
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("drain payload did not parse: {e}: {}", err.trim()))?;
    Ok(payload.get("undelivered").cloned().unwrap_or(Value::Null))
}

/// The parked-PR board fact: open parks with the remedy verb, read
/// in-process from the one owner so a second reader of the store shape can
/// never drift. An unreadable store reads empty, the same corrupt-reads-
/// empty posture the Python watcher store has always had.
fn r_parked() -> Result<Value, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd unreadable: {e}"))?;
    let ctx = crate::pr_park::Ctx::live(&cwd, crate::pr_park::Paths::resolve(&cwd));
    let rows = crate::pr_park::list_rows(&ctx);
    let open: Vec<Value> = rows
        .iter()
        .filter(|r| r.bucket == "open")
        .map(|r| {
            json!({
                "key": r.key,
                "node": r.node,
                "reason_detail": r.reason_detail,
                "age_hours": r.age_hours,
            })
        })
        .collect();
    Ok(json!({"open": open.len(), "rows": open}))
}

/// The control plane's own verdict: every arm failing past the notify
/// threshold, then the stuck-work findings, as the lines a page would carry.
/// Read in process - the same journals, predicate and threshold arm_watch
/// ticks with - so a check-in line and a page can never disagree.
fn r_control_plane(ctx: &Ctx) -> Result<Value, String> {
    let home = crate::paths::AgentsHome::from_env();
    let now_unix = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let journals = crate::tick_ledger::journals(&home);
    let mut rows = {
        let mut rows = crate::tick_ledger::read_arms(&journals, now_unix);
        crate::tick_ledger::fill_arm_values(&mut rows, &ctx.cwd);
        rows
    };
    let trace = crate::tick_ledger::read_tick_trace_live(&journals, &rows, now_unix);
    crate::tick_ledger::explain_with_trace(
        &mut rows,
        &crate::tick_ledger::DaemonFacts::Up {
            uptime_s: u64::MAX,
            drifted: false,
        },
        &trace,
    );
    let findings = crate::stuck_work::collect(&ctx.cwd)?;
    crate::arm_repair::annotate(&mut rows, &crate::arm_repair::RepairFacts::live(&findings));
    let threshold = crate::agents_config::notify_arm_failing_after_s(&ctx.cwd);
    let attention = control_plane_attention(&rows, &trace, &findings, threshold);
    Ok(json!({ "attention": attention }))
}

/// The check-in's control-plane attention lines, pure so the summary and
/// the row lines can never disagree. A deliberate hold (armed breaker or
/// hand pause) leads with one summary naming it; the overdue arms and
/// stuck-work findings follow unchanged. An unreadable breaker stays out
/// of the summary: its rows remain overdue faults.
fn control_plane_attention(
    rows: &[crate::tick_ledger::ArmStatus],
    trace: &crate::tick_ledger::TickTrace,
    findings: &[crate::stuck_work::Finding],
    threshold_s: u64,
) -> Vec<String> {
    let mut attention: Vec<String> = Vec::new();
    let paused: Vec<&crate::tick_ledger::ArmStatus> = rows
        .iter()
        .filter(|r| {
            matches!(
                r.cause.as_deref(),
                Some("fleet_stop") | Some("loops_paused")
            )
        })
        .collect();
    if let (false, Some(p)) = (paused.is_empty(), trace.pause.as_ref()) {
        let verb = match p {
            crate::loops_pause::DispatchPause::Manual { .. } => "fno do loops status",
            _ => "fno agents incident status",
        };
        // The tail reads the breaker's typed reach, never a fixed claim:
        // what is held and what proceeds come from the record the moment
        // the summary renders.
        let tail = match p {
            crate::loops_pause::DispatchPause::Manual { .. } => {
                "; loop dispatch is held; merges proceed".to_string()
            }
            crate::loops_pause::DispatchPause::FleetIncident { holds, .. } => {
                let admits: Vec<String> = crate::fleet_incident::SCOPES
                    .iter()
                    .filter(|s| !holds.contains(&(*s).to_string()))
                    .map(|s| s.to_string())
                    .collect();
                if admits.is_empty() {
                    "; the breaker holds every scope until it clears".to_string()
                } else {
                    format!(
                        "; the breaker holds {}; {} proceed until it clears",
                        holds.join(", "),
                        admits.join(" and ")
                    )
                }
            }
            // An unreadable breaker stays out of the summary (its rows stay
            // overdue faults), so this arm never renders.
            _ => String::new(),
        };
        attention.push(format!(
            "{} arms paused on purpose: {}{} ({verb})",
            paused.len(),
            p.detail(),
            tail
        ));
    }
    attention.extend(
        crate::arm_watch::overdue_arms(rows, threshold_s)
            .iter()
            .map(|row| row.line.trim().to_string()),
    );
    attention.extend(findings.iter().map(|f| f.line.clone()));
    attention
}

/// One territory row per scope: live against cap and the leadless mark,
/// read from the same projection the spawn
/// gate's cap enforces. An `membership: unknown` row is a failed reading, so
/// a blind spot prints `READER FAILED territory` instead of an empty table.
fn r_territory(ctx: &Ctx) -> Result<Value, String> {
    let registry = crate::paths::AgentsHome::from_env().registry_json();
    let rows = crate::territory::territory_rows(&ctx.cwd, &registry);
    if rows.len() == 1 && rows[0].get("membership").and_then(Value::as_str) == Some("unknown") {
        let reason = rows[0]
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("territory attribution unreadable");
        return Err(reason.to_string());
    }
    Ok(Value::Array(rows))
}

/// The state-root drift reading: undocumented top-level entries in the fno
/// state root. A failed doc or root read is a READER FAILED line, never a
/// silent zero (the unmeasured-state rule).
fn r_state_root_drift() -> Result<Value, String> {
    let home = crate::paths::AgentsHome::from_env();
    let state_root = crate::reclaim::reclaim_state_root(&home);
    let rep = crate::state_root_drift::drift_report(&state_root)?;
    Ok(json!({
        "undocumented": rep.count,
        "entries": rep.entries,
    }))
}

// ---------------------------------------------------------------------------
// gather

struct Beat {
    board: Result<Value, String>,
    folded: Result<Value, String>,
}

fn collect_readings(ctx: &Ctx, beat: &Beat, since: Option<&str>) -> Vec<Reading> {
    let mut readings: Vec<Reading> = Vec::new();
    let mut take = |name: &'static str, result: Result<Value, String>| {
        match result {
            Ok(value) => readings.push(Reading::took(name, value)),
            Err(error) => readings.push(Reading::failed(name, ReaderError(error).0)),
        };
    };
    take("user_notes", r_user_notes(ctx));
    take("board", r_board(&beat.board, &beat.folded, open_pr_count()));
    take("blueprint", {
        let (session_id, _harness) = crate::claims::resolve_identity();
        let claims = crate::claims::list(Some("node:"), None, false)
            .map(|rows| rows.iter().map(|r| r.holder.clone()).collect());
        let slots = crate::spawn_gate_lanes::share_reading(
            &crate::paths::AgentsHome::from_env().registry_json(),
            crate::agents_config::max_live(&ctx.cwd) as usize,
            session_id.as_deref(),
        )
        .share
        .ok_or_else(|| "lead share unreadable".to_string());
        let floor = crate::agents_config::config_lookup(&ctx.cwd, &["dispatch", "blueprint_floor"])
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| crate::backlog_ready::DEFAULT_BLUEPRINT_FLOOR.to_string());
        let other_owned =
            crate::lead_checkin_blueprint::other_owned_scopes(&beat.folded, &ctx.scope);
        crate::lead_checkin_blueprint::r_blueprint(
            &beat.board,
            &ctx.cwd,
            session_id,
            claims,
            slots,
            &floor,
            &other_owned,
        )
    });
    take(
        "escalations",
        std::env::current_dir()
            .map_err(|e| format!("unreadable (process cwd: {e})"))
            .and_then(|cwd| r_escalations(&cwd, &beat.folded)),
    );
    take("blocked_child", r_blocked_child(&beat.board));
    take("org", r_org(&beat.folded));
    take("territory", r_territory(ctx));
    take("state_root_drift", r_state_root_drift());
    take("capacity", r_capacity());
    take(
        "machine",
        crate::lead_checkin_machine::newest_reading(&ctx.events_paths),
    );
    let workers_payload = crate::lead_answers::fetch_workers_payload();
    take(
        "workers",
        workers_payload
            .as_ref()
            .map_err(Clone::clone)
            .and_then(crate::lead_answers::workers_summary),
    );
    take("watch_expiry", watch_projection::read());
    // The scope-answer readings share one scope compile and one top call.
    let scope_ids =
        crate::lead_answers::scope_node_ids(&ctx.graph, &ctx.cwd, &ctx.scope, ctx.level);
    take(
        "answered",
        scope_ids
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|ids| crate::lead_answers::answered_reading(&ctx.cwd, ids)),
    );
    take("quiet_workers", {
        let top = workers_payload.as_ref().ok();
        match (top, scope_ids.as_ref()) {
            (Some(top), Ok(ids)) => crate::lead_answers::quiet_reading(Some(top), ids),
            (None, _) => Err("the workers reading failed; quiet workers are unreadable".into()),
            (_, Err(e)) => Err(e.clone()),
        }
    });
    take("team", r_team());
    take("refusal_rate", r_refusal_rate());
    take("subagents", crate::lead_answers::r_subagents());
    take("wake_meter", r_wake_meter(since));
    take("drain", r_drain(ctx));
    take("held", crate::lead_answers::held_reading(&ctx.scope));
    take("repeated_asks", crate::repeated_asks::reading());
    take("skill_drift", crate::skill_drift::reading());
    take("main_ci", crate::main_ci::r_main_ci());
    take("control_plane", r_control_plane(ctx));
    take("self_hold", {
        crate::claims::resolve_identity()
            .0
            .ok_or_else(|| "current session identity is unavailable".to_string())
            .and_then(|session| crate::mail_hold::self_status(&session))
    });
    take("parked", r_parked());
    readings
}

// ---------------------------------------------------------------------------
// data, diff, change

fn build_data(readings: &[Reading], scope: &str) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("scope".into(), json!(scope));
    let get = |name: &str| readings.iter().find(|r| r.name == name);
    if let Some(board) = get("board").filter(|r| r.ok) {
        for key in ["open_prs", "free_claim_no_driver", "blocked"] {
            data.insert(
                key.into(),
                board.value.get(key).cloned().unwrap_or(Value::Null),
            );
        }
    }
    if let Some(bp) = get("blueprint").filter(|r| r.ok) {
        data.insert("blueprint_running".into(), bp.value["running"].clone());
        data.insert("blueprint_ceiling".into(), bp.value["ceiling"].clone());
        data.insert(
            "blueprint_plans_ready".into(),
            bp.value["plans_ready"].clone(),
        );
        data.insert("blueprint_slots".into(), bp.value["slots"].clone());
        data.insert("blueprint_starts".into(), bp.value["starts"].clone());
        data.insert(
            "blueprint_target_ready".into(),
            bp.value["target_ready"].clone(),
        );
        data.insert("blueprint_skips".into(), bp.value["skips"].clone());
    }
    if let Some(esc) = get("escalations").filter(|r| r.ok) {
        if esc.value.get("unreadable").is_none() {
            data.insert("escalations_open".into(), esc.value["open"].clone());
            data.insert("escalations_overdue".into(), esc.value["overdue"].clone());
        }
    }
    if let Some(child) = get("blocked_child").filter(|r| r.ok) {
        data.insert("blocked_children".into(), child.value.clone());
    }
    if let Some(org) = get("org").filter(|r| r.ok) {
        data.insert("owned_active".into(), org.value["owned_active"].clone());
    }
    if let Some(workers) = get("workers").filter(|r| r.ok) {
        data.insert("live_workers".into(), workers.value["live_workers"].clone());
        data.insert(
            "oldest_worker_seen".into(),
            workers.value["oldest_worker_seen"].clone(),
        );
        data.insert(
            "live_subagents".into(),
            workers.value["live_subagents"].clone(),
        );
    }
    if let Some(capacity) = get("capacity").filter(|r| r.ok) {
        for (key, wire) in [
            ("footprint", "capacity_footprint"),
            ("gate", "capacity_gate"),
            ("gate_axis", "capacity_gate_axis"),
            ("gate_cpu", "capacity_gate_cpu"),
            ("disagree", "capacity_disagree"),
            ("unparsed_lines", "unparsed_lines"),
            ("lanes", "capacity_lanes"),
        ] {
            data.insert(
                wire.into(),
                capacity.value.get(key).cloned().unwrap_or(Value::Null),
            );
        }
    }
    if let Some(rr) = get("refusal_rate").filter(|r| r.ok) {
        data.insert(
            "refusal_rate".into(),
            rr.value.get("rate").cloned().unwrap_or(Value::Null),
        );
    }
    if let Some(sa) = get("subagents").filter(|r| r.ok) {
        data.insert("idle_subagents".into(), sa.value["held_idle"].clone());
        let ids: Vec<Value> = sa
            .value
            .get("held")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("id").cloned())
                    .collect()
            })
            .unwrap_or_default();
        data.insert("idle_subagent_ids".into(), Value::Array(ids));
    }
    if let Some(wm) = get("wake_meter").filter(|r| r.ok) {
        data.insert("wake_machine".into(), wm.value["machine"].clone());
        data.insert("wake_user".into(), wm.value["user"].clone());
        data.insert("wake_ratio".into(), wm.value["ratio"].clone());
        data.insert("wake_over".into(), wm.value["over"].clone());
        data.insert(
            "subagent_tokens_since".into(),
            wm.value["tokens_since"].clone(),
        );
        data.insert(
            "subagent_tokens_session".into(),
            wm.value["tokens_session"].clone(),
        );
    }
    if let Some(drain) = get("drain").filter(|r| r.ok) {
        data.insert("undelivered".into(), drain.value.clone());
    }
    if let Some(held) = get("held").filter(|r| r.ok) {
        data.insert("held_open".into(), held.value["open"].clone());
    }
    watch_projection::add_data(readings, &mut data);
    if let Some(ci) = get("main_ci").filter(|r| r.ok) {
        data.insert("main_ci".into(), ci.value.clone());
    }
    if let Some(cp) = get("control_plane").filter(|r| r.ok) {
        data.insert(
            "control_plane_attention".into(),
            cp.value.get("attention").cloned().unwrap_or(json!([])),
        );
    }
    if let Some(hold) = get("self_hold").filter(|r| r.ok) {
        data.insert("self_hold".into(), hold.value.clone());
    }
    if let Some(sd) = get("skill_drift").filter(|r| r.ok) {
        let names: Vec<String> = sd
            .value
            .get("stale")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.get("name").and_then(Value::as_str))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if !names.is_empty() {
            data.insert("skill_drift_stale".into(), json!(names));
        }
    }
    let failed: Vec<&Reading> = readings.iter().filter(|r| !r.ok).collect();
    data.insert("coverage".into(), json!(readings.len() - failed.len()));
    data.insert(
        "readers_failed".into(),
        json!(failed.iter().map(|r| r.name).collect::<Vec<_>>()),
    );
    data
}

fn previous_row(ctx: &Ctx, holder: Option<&str>) -> (Option<Value>, String) {
    match crate::lead_history::previous_beat(&ctx.events_paths, &ctx.scope, holder, true) {
        Ok(first) => (first, String::new()),
        Err(e) => (None, e),
    }
}

/// Sets `refusal_rate_rising`: true only when the current rate exceeds the
/// last beat's, and that beat's exceeded the one before it. A single high
/// tick is noise; two consecutive rises is the handoff signal. The priors
/// come from [`crate::refusal_trend`], whose baseline advances with every
/// measured beat, journalled or not. A missing pair reads unmeasured, never
/// rising.
fn mark_refusal_rate_trend(
    data: &mut Map<String, Value>,
    previous_rate: Option<f64>,
    second_previous_rate: Option<f64>,
) {
    let current = data.get("refusal_rate").and_then(Value::as_f64);
    let rising = match (current, previous_rate, second_previous_rate) {
        (Some(c), Some(p1), Some(p2)) => c > p1 && p1 > p2,
        _ => false,
    };
    data.insert("refusal_rate_rising".into(), json!(rising));
    let unmeasured =
        current.is_some() && (previous_rate.is_none() || second_previous_rate.is_none());
    data.insert("refusal_rate_trend_unmeasured".into(), json!(unmeasured));
}

fn derive_change(
    previous_data: Option<&Value>,
    data: &Map<String, Value>,
    previous_error: &str,
) -> String {
    if !previous_error.is_empty() {
        return format!("previous beat unreadable: {previous_error}");
    }
    let mut moved: Vec<String> = Vec::new();
    if let Some(prev) = previous_data {
        let missing = missing_diff_keys(Some(prev));
        if !missing.is_empty() {
            return format!("unmeasured: previous row lacks {}", missing.join(", "));
        }
        for key in NUMERIC_DIFF_KEYS {
            let before = prev.get(key);
            let after = data.get(key);
            if let (Some(before), Some(after)) = (before, after) {
                if before != after {
                    moved.push(format!("{key} {before} -> {after}"));
                }
            }
        }
    }
    let mut attention: Vec<String> = data
        .get("control_plane_attention")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let self_hold = data.get("self_hold");
    if self_hold
        .and_then(|hold| hold.get("clock_live"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || self_hold
            .and_then(|hold| hold.get("delivery_policy"))
            .and_then(Value::as_str)
            == Some("bus-only")
    {
        attention.push("DND on".into());
    }
    let stale_skills: Vec<&str> = data
        .get("skill_drift_stale")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !stale_skills.is_empty() {
        attention.push(format!(
            "skill text stale since compaction: {}",
            stale_skills.join(", ")
        ));
    }
    if data
        .get("refusal_rate_rising")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        attention.push("refusal rate rising two consecutive beats".into());
    }
    if data
        .get("wake_over")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let item = match data.get("wake_ratio").and_then(Value::as_f64) {
            Some(ratio) => format!("wake ratio {ratio:.1} to 1 over 3 to 1"),
            None => "wake ratio n/a (no typed turns) over 3 to 1".to_string(),
        };
        attention.push(item);
    }
    if data
        .get("idle_subagents")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        attention.push(format!(
            "{} finished subagents held unstopped",
            data.get("idle_subagents")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        ));
    }
    // Attention outranks silence: a control plane failing for 30 minutes,
    // or a refusal rate climbing two beats running, is never journaled as
    // "no change", whatever the counts did.
    if !attention.is_empty() {
        let moved_suffix = if moved.is_empty() {
            String::new()
        } else {
            format!("; moved: {}", moved.join(", "))
        };
        return format!("attention: {}{moved_suffix}", attention.join("; "));
    }
    if !moved.is_empty() {
        return format!("moved: {}", moved.join(", "));
    }
    let failed = data
        .get("readers_failed")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    if !failed.is_empty() {
        return format!(
            "no numeric movement; readings failed: {}",
            failed.join(", ")
        );
    }
    if previous_data.is_none() {
        return "first canonical beat for this scope".into();
    }
    "no change".into()
}

/// The row's change: the lead's sentence when one was given, else the
/// derived diff. The derivation always lands under `diff`, so a
/// model-worded row still carries the machine's measurement.
fn finish_change(derived: String, model: Option<&str>, data: &mut Map<String, Value>) -> String {
    let change = model
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| derived.clone());
    data.insert("diff".into(), json!(derived));
    data.insert("change".into(), json!(change.clone()));
    change
}

// ---------------------------------------------------------------------------
// render

pub(crate) fn dash(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn render_lines(
    scope: &str,
    readings: &[Reading],
    data: &Map<String, Value>,
    previous: &Option<Value>,
    previous_error: &str,
    change: &str,
) -> Vec<String> {
    let by_name = |name: &str| readings.iter().find(|r| r.name == name);
    let failed = |name: &str| readings.iter().find(|r| r.name == name && !r.ok);
    let mut lines: Vec<String> = Vec::new();

    if let Some(r) = by_name("machine") {
        if r.ok {
            lines.push(crate::lead_checkin_machine::beat_line(&r.value));
        } else {
            lines.push(format!("READER FAILED machine: {}", r.error));
        }
    }

    match by_name("user_notes") {
        Some(r) if r.ok && !r.value.is_null() => {
            lines.push("User notes:".into());
            for l in r
                .value
                .as_str()
                .unwrap_or("")
                .trim_end_matches('\n')
                .split('\n')
            {
                lines.push(l.to_string());
            }
        }
        Some(r) if !r.ok => lines.push(format!("READER FAILED user_notes: {}", r.error)),
        _ => {}
    }

    match failed("board") {
        Some(r) => lines.push(format!("READER FAILED board: {}", r.error)),
        None => {
            let blocked_on = by_name("board")
                .and_then(|r| r.value.get("blocked_on"))
                .and_then(|o| o.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            let mut text = format!(
                "board: open_prs {}, free_claim_no_driver {}, blocked {}",
                data.get("open_prs")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".into()),
                data.get("free_claim_no_driver")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".into()),
                data.get("blocked")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".into()),
            );
            if !blocked_on.is_empty() {
                text.push_str(&format!(" (on: {})", blocked_on.join("; ")));
            }
            lines.push(text);
        }
    }

    match failed("blueprint") {
        Some(r) => lines.push(format!("READER FAILED blueprint: {}", r.error)),
        None => {
            let bp = by_name("blueprint")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            lines.push(format!(
                "blueprint: running {} of {} ({}); plans ready {} / slots {}",
                dash(bp.get("running")),
                dash(bp.get("ceiling")),
                dash(bp.get("ceiling_source")),
                dash(bp.get("plans_ready")),
                dash(bp.get("slots")),
            ));
            for id in bp
                .get("starts")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
            {
                lines.push(format!(
                    "  start /fno:blueprint subagent {}",
                    dash(Some(id))
                ));
            }
            for id in bp
                .get("target_ready")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
            {
                lines.push(format!("  target-ready: /fno:target {}", dash(Some(id))));
            }
            for skip in bp
                .get("skips")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
            {
                lines.push(format!(
                    "  skip {} : {}",
                    dash(skip.get("id")),
                    dash(skip.get("reason"))
                ));
            }
        }
    }

    match failed("escalations") {
        Some(r) => lines.push(format!("READER FAILED escalations: {}", r.error)),
        None => {
            let reading = by_name("escalations")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            let rows = reading
                .get("rows")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            let open = reading.get("open").and_then(|v| v.as_i64()).unwrap_or(0);
            let overdue = reading.get("overdue").and_then(|v| v.as_i64()).unwrap_or(0);
            lines.push(format!("escalations: open {open}, overdue {overdue}"));
            for row in rows.iter().take(MAX_ORG_ROWS) {
                lines.push(format!(
                    "  {} ({}), deadline {}, {}",
                    dash(row.get("title")),
                    dash(row.get("class")),
                    dash(row.get("deadline")),
                    dash(row.get("state")),
                ));
            }
        }
    }

    match failed("blocked_child") {
        Some(r) => lines.push(format!("READER FAILED blocked_child: {}", r.error)),
        None => {
            let rows = data
                .get("blocked_children")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            lines.push(format!("blocked_child: {}", rows.len()));
            for row in &rows {
                let reason = s_str(row, "reason")
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                lines.push(format!(
                    "  {}, session {}, age {}m{}",
                    dash(row.get("node")),
                    dash(row.get("session")),
                    dash(row.get("age_minutes")),
                    reason
                ));
            }
        }
    }

    lines.extend(crate::lead_answers::answered_lines(readings));

    lines.extend(crate::lead_answers::held_lines(readings));
    lines.extend(crate::repeated_asks::lines(readings));
    lines.extend(crate::skill_drift::lines(readings));

    match failed("org") {
        Some(r) => lines.push(format!("READER FAILED org: {}", r.error)),
        None => {
            let org = by_name("org").map(|r| &r.value).unwrap_or(&Value::Null);
            let active = org
                .get("active_nodes")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into());
            let total = dash(org.get("total_nodes"));
            match data.get("owned_active") {
                Some(v) if !v.is_null() => lines.push(format!(
                    "{scope}: {v} owned active of {active} active, {total} nodes"
                )),
                _ => {
                    let reason = org.get("owned_reason").and_then(Value::as_str);
                    let unmeasured = match reason {
                        Some(r) if !r.is_empty() => format!("owned unmeasured ({r})"),
                        _ => "owned unmeasured".to_string(),
                    };
                    lines.push(format!(
                        "{scope}: {unmeasured}, {active} active, {total} nodes"
                    ));
                }
            }
            lines.push(epic_line(org));
            let rows = org
                .get("rows")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            for row in rows.iter().take(MAX_ORG_ROWS) {
                lines.push(format!(
                    "  {} {} worker {} pr {} session {}",
                    dash(row.get("id")),
                    dash(row.get("status")),
                    dash(row.get("worker")),
                    dash(row.get("pr_number")),
                    dash(row.get("session")),
                ));
            }
            let hidden = rows.len().saturating_sub(MAX_ORG_ROWS);
            if hidden > 0 {
                lines.push(format!("  ... {hidden} more rows cut"));
            }
        }
    }

    lines.extend(crate::lead_answers::quiet_lines(readings));

    match failed("territory") {
        Some(r) => lines.push(format!("READER FAILED territory: {}", r.error)),
        None => {
            let territory = by_name("territory")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            let rows = territory.as_array().cloned().unwrap_or_default();
            lines.push(format!("territory: {} scopes", rows.len()));
            for row in rows.iter().take(MAX_ORG_ROWS) {
                lines.push(format!(
                    "  {} rung {} mission {} live {}/{}{}{}",
                    dash(row.get("scope")),
                    dash(row.get("rung")),
                    dash(row.get("mission")),
                    dash(row.get("live")),
                    dash(row.get("cap")),
                    row["leadless"]
                        .as_bool()
                        .filter(|v| *v)
                        .map(|_| " leadless")
                        .unwrap_or(""),
                    if row["membership"] == "unknown" {
                        row["reason"]
                            .as_str()
                            .map(|r| format!(" unreadable ({r})"))
                            .unwrap_or_else(|| " unreadable".to_string())
                    } else {
                        String::new()
                    },
                ));
            }
            let hidden = rows.len().saturating_sub(MAX_ORG_ROWS);
            if hidden > 0 {
                lines.push(format!("  ... {hidden} more rows cut"));
            }
        }
    }

    match failed("capacity") {
        Some(r) => lines.push(format!("READER FAILED capacity: {}", r.error)),
        None => {
            let mut text = format!(
                "capacity: footprint {} / gate {}",
                dash(data.get("capacity_footprint")),
                dash(data.get("capacity_gate")),
            );
            if let Some(axis) = data.get("capacity_gate_axis").and_then(Value::as_str) {
                text.push_str(&format!(" on {axis}"));
            }
            text.push_str(&format!(", cpu {}", dash(data.get("capacity_gate_cpu"))));
            if data
                .get("capacity_disagree")
                .and_then(|d| d.as_bool())
                .unwrap_or(false)
            {
                text.push_str(" DISAGREE");
            }
            let unparsed = data
                .get("unparsed_lines")
                .and_then(|u| u.as_i64())
                .unwrap_or(0);
            if unparsed != 0 {
                text.push_str(&format!(
                    " (floor: {unparsed} ps row(s) unparsed, their CPU missing, so this admits permissively; fno doctor footprint --json, read .unparsed_samples)"
                ));
            }
            text.push_str(&format!(" | lanes {}", dash(data.get("capacity_lanes"))));
            lines.push(text);
        }
    }

    if let Some(error) = watch_projection::read_error(readings) {
        lines.push(format!("READER FAILED watch expiry: {error}"));
    }
    match failed("workers") {
        Some(r) => {
            lines.push(format!("READER FAILED workers: {}", r.error));
            lines.push(format!("worker activity unmeasured: {}", r.error));
        }
        None => {
            let mut line = format!(
                "workers: live {}, oldest activity {}, subagents active {}",
                dash(data.get("live_workers")),
                dash(data.get("oldest_worker_seen")),
                dash(data.get("live_subagents")),
            );
            line.push_str(&watch_projection::workers_suffix(readings, data));
            lines.push(line);
        }
    }
    lines.extend(crate::lead_answers::subagent_lines(readings));

    match failed("team") {
        Some(r) => lines.push(format!("READER FAILED team: {}", r.error)),
        None => {
            let team = by_name("team").map(|r| &r.value).unwrap_or(&Value::Null);
            lines.push(format!(
                "team: {} teams, double-ruled {}, stale-teamed {}, manifest-splits {}, disagreements {}",
                dash(team.get("total")),
                dash(team.get("double_ruled")),
                dash(team.get("stale_teamed")),
                dash(team.get("splits")),
                dash(team.get("disagreements")),
            ));
            for anomaly in team
                .get("anomalies")
                .and_then(|a| a.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(text) = anomaly.as_str() {
                    lines.push(format!("  {text}"));
                } else {
                    lines.push(format!(
                        "  {}",
                        s_str(anomaly, "scope")
                            .unwrap_or(&anomaly.to_string())
                            .to_string()
                    ));
                }
            }
        }
    }
    match failed("refusal_rate") {
        Some(r) => lines.push(format!("READER FAILED refusal_rate: {}", r.error)),
        None => {
            let rr = by_name("refusal_rate")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            let rate = rr.get("rate").and_then(Value::as_f64).unwrap_or(0.0);
            let mut text = format!(
                "refusal_rate: {:.1}% ({}/{} last {} calls)",
                rate * 100.0,
                dash(rr.get("refused")),
                dash(rr.get("total")),
                dash(rr.get("window")),
            );
            let rising = data
                .get("refusal_rate_rising")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let unmeasured = data
                .get("refusal_rate_trend_unmeasured")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if rising {
                text.push_str(" - RISING (handoff signal)");
            } else if unmeasured {
                text.push_str(" - UNMEASURED (needs two prior beats)");
            }
            lines.push(text);
        }
    }

    match failed("wake_meter") {
        Some(r) => lines.push(format!("READER FAILED wake_meter: {}", r.error)),
        None => {
            let wm = by_name("wake_meter")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            let machine = wm.get("machine").and_then(Value::as_u64).unwrap_or(0);
            let user = wm.get("user").and_then(Value::as_u64).unwrap_or(0);
            let mut text = if user == 0 {
                format!("wake_ratio: {machine} machine / 0 user wakes = n/a")
            } else {
                format!(
                    "wake_ratio: {machine} machine / {user} user wakes = {:.1} to 1",
                    machine as f64 / user as f64
                )
            };
            if data
                .get("wake_over")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                text.push_str(" - OVER 3 to 1");
            }
            lines.push(text);
            let since_phrase = if previous
                .as_ref()
                .and_then(|p| p.get("ts"))
                .and_then(Value::as_str)
                .is_some()
            {
                "since last beat"
            } else {
                "since session start"
            };
            lines.push(format!(
                "subagent_tokens: {} {since_phrase} ({} this session)",
                dash(wm.get("tokens_since")),
                dash(wm.get("tokens_session")),
            ));
        }
    }

    match failed("drain") {
        Some(r) => lines.push(format!("READER FAILED drain: {}", r.error)),
        None => lines.push(format!(
            "drain: undelivered {}",
            dash(data.get("undelivered"))
        )),
    }
    match failed("main_ci") {
        Some(r) => lines.push(format!("READER FAILED main_ci: {}", r.error)),
        None => {
            lines.push(format!(
                "main ci: {}",
                crate::main_ci::main_ci_render(data.get("main_ci"))
            ));
            for line in crate::main_ci::main_ci_stale_lines(data.get("main_ci"), chrono::Utc::now())
            {
                lines.push(line);
            }
        }
    }
    match failed("control_plane") {
        Some(r) => lines.push(format!("READER FAILED control_plane: {}", r.error)),
        None => {
            let attention: Vec<&str> = data
                .get("control_plane_attention")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if attention.is_empty() {
                lines.push("control plane: ok".into());
            } else {
                lines.push("control plane:".into());
                for entry in attention {
                    lines.push(format!("  {entry}"));
                }
            }
        }
    }
    match failed("self_hold") {
        Some(r) => lines.push(format!("READER FAILED self_hold: {}", r.error)),
        None => {
            let hold = by_name("self_hold")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            let clock_live = hold
                .get("clock_live")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let clock = match (clock_live, hold.get("clock_until").and_then(Value::as_str)) {
                (true, Some(until)) => format!("clock active until {until}"),
                (true, None) => "clock active".into(),
                (false, Some(until)) => format!("clock inactive (until {until})"),
                (false, None) => "clock inactive".into(),
            };
            let policy = hold
                .get("delivery_policy")
                .and_then(Value::as_str)
                .unwrap_or("none");
            lines.push(format!("self_hold: {clock}; delivery_policy {policy}"));
            if clock_live || policy == "bus-only" {
                lines.push("attention: DND on".into());
            }
        }
    }
    match failed("state_root_drift") {
        Some(r) => lines.push(format!("READER FAILED state_root_drift: {}", r.error)),
        None => {
            let drift = by_name("state_root_drift")
                .map(|r| &r.value)
                .unwrap_or(&Value::Null);
            let count = drift
                .get("undocumented")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if count == 0 {
                lines.push("state_root_drift: clean".into());
            } else {
                let entries: Vec<String> = drift
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .take(5)
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if count > 5 { ", ..." } else { "" };
                lines.push(format!(
                    "state_root_drift: {count} undocumented top-level entries: {}{more}",
                    entries.join(", ")
                ));
            }
        }
    }
    match failed("parked") {
        Some(r) => lines.push(format!("READER FAILED parked: {}", r.error)),
        None => {
            let rows = by_name("parked")
                .and_then(|r| r.value.get("rows"))
                .and_then(|o| o.as_array())
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                lines.push("parked: none".into());
            } else {
                lines.push("parked:".into());
                for row in rows {
                    let key = row.get("key").and_then(Value::as_str).unwrap_or("?");
                    let node = row.get("node").and_then(Value::as_str).unwrap_or("-");
                    let detail = row
                        .get("reason_detail")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let age = row.get("age_hours").and_then(Value::as_i64).unwrap_or(-1);
                    let age_s = if age < 0 {
                        "?".to_string()
                    } else {
                        format!("{age}h")
                    };
                    lines.push(format!(
                        "  {key} {detail} ({age_s}, node {node}); remedy: fno-agents pr-park unpark {key}"
                    ));
                }
            }
        }
    }

    let failed_names: Vec<&str> = data
        .get("readers_failed")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let coverage = data.get("coverage").and_then(|c| c.as_i64()).unwrap_or(0);
    let ran = coverage + failed_names.len() as i64;
    lines.push(format!("coverage: {coverage} of {ran} readings ok"));
    if !failed_names.is_empty() {
        let named = failed_names
            .iter()
            .map(|name| {
                let error = failed(name).map(|r| r.error.as_str()).unwrap_or("");
                format!("{name} ({error})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("failed readers: {named}"));
    }

    if !previous_error.is_empty() {
        lines.push(format!("vs last beat: unmeasured ({previous_error})"));
    } else if previous.is_none() {
        lines.push("vs last beat: none, this is the first canonical beat for this scope".into());
    } else {
        let prev_data = previous
            .as_ref()
            .and_then(|p| p.get("data"))
            .cloned()
            .unwrap_or(json!({}));
        let ts = previous.as_ref().and_then(|p| s_str(p, "ts")).unwrap_or("");
        let missing = missing_diff_keys(Some(&prev_data));
        if !missing.is_empty() {
            lines.push(format!(
                "vs last beat: unmeasured (previous row lacks {})",
                missing.join(", ")
            ));
        } else {
            let parts: Vec<String> = NUMERIC_DIFF_KEYS
                .iter()
                .filter_map(|key| {
                    let before = prev_data.get(*key)?;
                    let after = data.get(*key)?;
                    Some(format!("{key} {before} -> {after}"))
                })
                .collect();
            lines.push(format!("vs last beat ({ts}): {}", parts.join(", ")));
        }
    }
    lines.push(format!("change: {change}"));
    lines
}

// ---------------------------------------------------------------------------
// faq prompt + emit

fn faq_entries_for_scope(faqs_dir: &Path, scope: &str) -> Vec<String> {
    let mut entries: Vec<(SystemTime, String)> = Vec::new();
    let Ok(it) = std::fs::read_dir(faqs_dir) else {
        return Vec::new();
    };
    for entry in it.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("lead-") || !name.ends_with(".md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if frontmatter_scope(&text) == Some(scope) {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            entries.push((mtime, text.trim().to_string()));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().map(|(_, t)| t).collect()
}

/// The `scope:` line of a simple `---` frontmatter block, or None.
fn frontmatter_scope(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return None;
        }
        if let Some(rest) = line.strip_prefix("scope:") {
            return rest
                .trim()
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .or(Some(rest.trim()));
        }
    }
    None
}

const FAQ_PROMPT: &str = "fno agents lead faq add --question \"...\" --answer \"...\" \
--specimen \"<node or PR>, <date>\" --exit \"<the change that retires this>\"";

/// The one `lead_checkin` writer. `source` is the emitting half (`loop` for
/// the verb's own beat, `hook` for the stop hook's missed-beat row); the
/// append runs through the capped, rotation-safe `EventEmitter`, so the row
/// never bypasses the payload cap or the ingest-before-rotate guard.
pub(crate) fn emit_row(path: &Path, source: &str, data: &Map<String, Value>) -> bool {
    // The store stamps a lead row's scope only when it is a canonical team
    // scope; emitting one that is not would be unfindable by --scope forever
    // (the 2026-09-14 rendered-board corruption), so the one writer refuses.
    let Some(scope) = data.get("scope").and_then(|v| v.as_str()) else {
        eprintln!(
            "lead-checkin: WARNING: lead_checkin row not emitted: data carries no scope string"
        );
        return false;
    };
    let scope_canonical = !scope.is_empty()
        && crate::territory::canonical_scope(scope) == scope
        && !scope
            .split(',')
            .any(|m| m.is_empty() || m.chars().any(char::is_whitespace));
    if !scope_canonical {
        eprintln!(
            "lead-checkin: WARNING: lead_checkin row not emitted: scope {scope:?} is not a canonical team scope"
        );
        return false;
    }
    let forbidden = ["crown", "crown_scope", "result"]
        .iter()
        .any(|k| data.contains_key(*k));
    if forbidden {
        eprintln!(
            "lead-checkin: WARNING: lead_checkin row not emitted: a forbidden alias key is present"
        );
        return false;
    }
    match crate::events::EventEmitter::new(path, source).emit_fields(LEAD_CHECKIN, data.clone()) {
        Ok(()) => true,
        Err(e) => {
            // One write, one truth: a failed row is warned, never retried.
            // A retry that re-appends after a partial write would journal
            // the same beat twice, and the diff corpus cannot unsee that.
            eprintln!("lead-checkin: WARNING: lead_checkin row not emitted: {e}");
            false
        }
    }
}

fn finish_checkin(
    emit_requested: bool,
    emitted: bool,
    output_error: Option<std::io::Error>,
) -> i32 {
    if let Some(error) = output_error {
        if error.kind() == std::io::ErrorKind::BrokenPipe && emitted {
            return 0;
        }
        eprintln!("lead-checkin: stdout write failed: {error}");
        return 3;
    }
    if emit_requested && !emitted {
        eprintln!(
            "lead-checkin: beat ran but no lead_checkin row was journalled; fno agents lead history will not see it"
        );
        return 3;
    }
    0
}

// ---------------------------------------------------------------------------
// entry

/// Fill in the caller-team inputs the retired Python shell used to resolve:
/// the scope when `--scope` was not passed (refusing when the team cannot
/// be resolved), the team level from the registry row holding that scope,
/// and the board-state manifest for the named scope when one exists. The
/// registry read tolerates unknown keys, so a row carrying a field this
/// binary predates no longer blinds the resolution.
fn resolve_missing_team_inputs(
    scope: &mut String,
    level: &mut Option<i64>,
    board_state: &mut Option<PathBuf>,
    cwd: &Path,
) -> Result<(), String> {
    let registry_path = crate::paths::AgentsHome::from_env().registry_json();
    if scope.is_empty() {
        *scope = crate::lead_verdict_inputs::resolve_scope(None, &registry_path)?;
    }
    if level.is_none() {
        match crate::state::load_registry(&registry_path) {
            Ok(registry) => {
                *level = registry
                    .entries
                    .iter()
                    .find(|row| {
                        row.crown_level.is_some()
                            && row
                                .crown_scope
                                .as_deref()
                                .map(|s| crate::territory::canonical_scope(s.trim()) == *scope)
                                .unwrap_or(false)
                    })
                    .and_then(|row| row.crown_level)
                    .map(i64::from);
            }
            Err(e) => {
                eprintln!(
                    "lead-checkin: the agent registry could not be read, so the \
                     team level is unresolved: {e}"
                );
            }
        }
    }
    if board_state.is_none() {
        if let Ok(path) = crate::lead_state::manifest_path(&crate::paths::space_dir(cwd), scope) {
            if path.is_file() {
                *board_state = Some(path);
            }
        }
    }
    Ok(())
}

/// `lead-checkin [--scope SCOPE] --events-path PATH [--events-path ...]
///              --graph PATH --handoffs-dir PATH [--faqs-dir PATH]
///              [--board-state PATH] [--emit-path PATH] [--change TEXT]
///              [--no-emit] [--json]`
///
/// With no `--scope`, the caller's team scope, level and board state are
/// resolved natively from the registry (the retired Python shell's job).
///
/// rc 0 a completed beat, 3 when an asked-for row was not journalled or
/// stdout could not be written, 2 usage failure.
pub fn run_lead_checkin(args: &[String]) -> i32 {
    let mut ctx = Ctx {
        scope: String::new(),
        level: None,
        events_paths: Vec::new(),
        graph: PathBuf::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        handoffs_dir: PathBuf::new(),
        faqs_dir: None,
        board_state: None,
        emit_path: None,
        emit: true,
    };
    let mut as_json = false;
    let mut model_change: Option<String> = None;
    let mut team_name: Option<String> = None;
    let mut theme: Option<String> = None;
    let mut keep_name_from: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let flag = |name: &str| args[i] == name && i + 1 < args.len();
        if flag("--scope") {
            ctx.scope = args[i + 1].clone();
            i += 2;
        } else if flag("--change") {
            model_change = Some(args[i + 1].clone());
            i += 2;
        } else if flag("--name") {
            team_name = Some(args[i + 1].clone());
            i += 2;
        } else if flag("--theme") {
            theme = Some(args[i + 1].clone());
            i += 2;
        } else if flag("--keep-name-from") {
            keep_name_from = Some(args[i + 1].clone());
            i += 2;
        } else if flag("--events-path") {
            ctx.events_paths.push(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if flag("--graph") {
            ctx.graph = PathBuf::from(&args[i + 1]);
            i += 2;
        } else if flag("--cwd") {
            ctx.cwd = PathBuf::from(&args[i + 1]);
            i += 2;
        } else if flag("--level") {
            ctx.level = args[i + 1].parse::<i64>().ok();
            i += 2;
        } else if flag("--handoffs-dir") {
            ctx.handoffs_dir = PathBuf::from(&args[i + 1]);
            i += 2;
        } else if flag("--faqs-dir") {
            ctx.faqs_dir = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if flag("--board-state") {
            ctx.board_state = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if flag("--emit-path") {
            ctx.emit_path = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if args[i] == "--no-emit" {
            ctx.emit = false;
            i += 1;
        } else if args[i] == "--json" {
            as_json = true;
            i += 1;
        } else {
            eprintln!("fno-agents lead-checkin: unknown flag {}", args[i]);
            eprintln!(
                "fno-agents lead-checkin: --scope SCOPE --events-path PATH \
                 [--events-path ...] --graph PATH --handoffs-dir PATH \
                 [--faqs-dir PATH] [--board-state PATH] [--emit-path PATH] \
                 [--change TEXT] [--name NAME] [--theme THEME] \
                 [--keep-name-from OLD-SCOPE] \
                 [--no-emit] [--json]"
            );
            return 2;
        }
    }
    if ctx.events_paths.is_empty()
        || ctx.graph.as_os_str().is_empty()
        || ctx.handoffs_dir.as_os_str().is_empty()
    {
        eprintln!(
            "fno-agents lead-checkin: --events-path, --graph and \
             --handoffs-dir are required"
        );
        return 2;
    }
    if let Err(msg) = resolve_missing_team_inputs(
        &mut ctx.scope,
        &mut ctx.level,
        &mut ctx.board_state,
        &ctx.cwd,
    ) {
        eprintln!("lead: {msg}");
        return 2;
    }

    let ts = iso_now();
    let home = crate::paths::AgentsHome::from_env();
    if let Err(e) = crate::team_names::apply_team_naming(
        &home.team_names_json(),
        &home.registry_json(),
        team_name.as_deref(),
        keep_name_from.as_deref(),
        theme.as_deref(),
        ctx.level,
        &ctx.scope,
    ) {
        eprintln!("fno-agents lead-checkin: {e}");
        return 2;
    }
    if let Err(error) = rename_harness_title_for_team(&ctx.scope) {
        eprintln!("fno-agents lead-checkin: harness title rename failed: {error}");
    }
    // The beat stamps the lead clock first: the holder session it names is
    // what the previous-beat lookup and the refusal trend key on.
    let holder = crate::team_names::stamp_beat_lead(&home.team_names_json(), &ctx.cwd, &ctx.scope);
    let (previous, previous_error) = previous_row(&ctx, holder.as_deref());
    let since = previous
        .as_ref()
        .and_then(|p| p.get("ts"))
        .and_then(Value::as_str);
    let beat = Beat {
        board: fetch_board(&ctx),
        folded: fetch_fold(&ctx),
    };
    let readings = collect_readings(&ctx, &beat, since);
    let mut data = build_data(&readings, &ctx.scope);
    if let Some(holder) = holder.as_deref() {
        data.insert("holder_session".into(), json!(holder));
    }
    let previous_data = previous.as_ref().and_then(|p| p.get("data"));
    let trend_dir = home.refusal_trend_dir();
    let trend_key = holder.as_deref().unwrap_or(&ctx.scope);
    let (previous_rate, second_previous_rate) = crate::refusal_trend::priors(&trend_dir, trend_key);
    mark_refusal_rate_trend(&mut data, previous_rate, second_previous_rate);
    // The baseline advances on the measurement the beat just printed,
    // whether or not the full row journals below.
    if let Some(rate) = data.get("refusal_rate").and_then(Value::as_f64) {
        crate::refusal_trend::record(&trend_dir, trend_key, &ts, rate);
    }
    let derived = derive_change(previous_data, &data, &previous_error);
    let change = finish_change(derived.clone(), model_change.as_deref(), &mut data);
    let mut lines = render_lines(
        &ctx.scope,
        &readings,
        &data,
        &previous,
        &previous_error,
        &change,
    );
    // The team line leads: identity first, then the beat's facts. The name
    // comes from the fold's own stamp (org_fold reads the store), so an
    // heir's first beat already shows the carried name.
    let fold_name: Option<String> = beat.folded.as_ref().ok().and_then(|f| {
        f.get("fold")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    let level_txt = ctx
        .level
        .map(|l| format!("L{l}"))
        .unwrap_or_else(|| "L?".to_string());
    let title_txt = crate::team_names::stored_title(
        &crate::paths::AgentsHome::from_env().team_names_json(),
        &ctx.scope,
    )
    .unwrap_or_else(|| format!("{level_txt} {}", ctx.scope));
    lines.insert(
        0,
        crate::team_names::team_line_text(fold_name.as_deref(), &title_txt, &ctx.scope),
    );
    // Bind an unbound record to the live holder's session and refresh the
    // node list from this beat's fold. A store write failure is a stated
    // line in the beat, never a failed beat.
    let owned_ids: Vec<String> = beat
        .folded
        .as_ref()
        .ok()
        .and_then(|f| {
            f.get("fold")
                .and_then(|f| f.get("nodes"))
                .and_then(|n| n.as_array())
                .map(|rows| {
                    rows.iter()
                        .filter(|n| n.get("owned") != Some(&Value::Bool(false)))
                        .filter_map(|n| n.get("id").and_then(|v| v.as_str()).map(String::from))
                        .collect::<Vec<String>>()
                })
        })
        .unwrap_or_default();
    if let Err(e) = crate::team_names::bind_and_refresh(
        &crate::paths::AgentsHome::from_env().team_names_json(),
        &crate::paths::AgentsHome::from_env().registry_json(),
        &ctx.scope,
        owned_ids,
    ) {
        lines.push(format!("team name: {e}"));
    }
    if model_change.as_deref().map(|t| !t.trim().is_empty()) == Some(true) {
        lines.push(format!("diff: {derived}"));
    }

    let readers_failed: Vec<String> = data
        .get("readers_failed")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let faq_needed = !readers_failed.is_empty()
        || ctx
            .faqs_dir
            .as_deref()
            .map(|dir| faq_entries_for_scope(dir, &ctx.scope).is_empty())
            .unwrap_or(true);
    if faq_needed {
        lines.push(FAQ_PROMPT.into());
    }

    let emitted = if ctx.emit {
        match ctx.emit_path.as_ref() {
            Some(path) => emit_row(path, "loop", &data),
            None => {
                eprintln!("lead-checkin: WARNING: no emit path, so the beat was not journalled");
                false
            }
        }
    } else {
        false
    };

    let output_error = if as_json {
        let payload = json!({
            "scope": ctx.scope,
            "ts": ts,
            "name": fold_name,
            "coverage": data.get("coverage").cloned().unwrap_or(json!(0)),
            "readers_failed": readers_failed,
            "change": change,
            "diff": derived,
            "previous_ts": previous.as_ref().and_then(|p| s_str(p, "ts")),
            "previous_error": previous_error,
            "emitted": emitted,
            "data": Value::Object(data.clone()),
            "readings": readings.iter().map(|r| {
                let mut row = Map::new();
                row.insert("name".into(), json!(r.name));
                row.insert("ok".into(), json!(r.ok));
                if r.ok {
                    row.insert("value".into(), r.value.clone());
                } else {
                    row.insert("error".into(), json!(r.error));
                }
                Value::Object(row)
            }).collect::<Vec<_>>(),
            "lines": lines,
        });
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        )
        .err()
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let mut error: Option<std::io::Error> = None;
        for line in &lines {
            if let Err(e) = writeln!(out, "{line}") {
                error = Some(e);
                break;
            }
        }
        error
    };
    finish_checkin(ctx.emit, emitted, output_error)
}

pub(crate) fn title_rename_command(
    harness: &str,
    label: &str,
    title: Option<&str>,
) -> Option<String> {
    if title == Some(label) {
        return None;
    }
    let contract = crate::harness_capabilities::HarnessContract::packaged().ok()?;
    let capabilities = contract.capabilities(harness).ok()?;
    capabilities
        .native_verbs
        .iter()
        .any(|verb| verb == "/rename")
        .then(|| format!("/rename {label}"))
}

fn rename_harness_title_for_team(scope: &str) -> Result<(), String> {
    let home = crate::paths::AgentsHome::from_env();
    let registry = crate::state::load_registry(&home.registry_json())
        .map_err(|error| format!("registry read failed: {error}"))?;
    let canonical = crate::territory::canonical_scope(scope);
    let mut holders = registry.entries.iter().filter(|row| {
        row.crown_scope
            .as_deref()
            .is_some_and(|row_scope| crate::territory::canonical_scope(row_scope) == canonical)
            && !matches!(
                row.status,
                crate::AgentStatus::Exited
                    | crate::AgentStatus::Orphaned
                    | crate::AgentStatus::Failed
                    | crate::AgentStatus::PermanentDead
            )
    });
    let row = holders
        .next()
        .ok_or_else(|| format!("no live holder for {canonical}"))?;
    if holders.next().is_some() {
        return Err(format!("multiple live holders for {canonical}"));
    }
    let session = row
        .harness_session_id
        .as_deref()
        .filter(|session| !session.is_empty())
        .ok_or_else(|| format!("{} has no harness session id", row.name))?;
    let harness = row
        .harness
        .as_deref()
        .filter(|harness| !harness.is_empty())
        .ok_or_else(|| format!("{} has no harness", row.name))?;
    if crate::claims::resolve_identity().0.as_deref() != Some(session) {
        return Err("the live team holder is not this session".into());
    }
    let label = row.name.as_str();
    let Some(command) = title_rename_command(harness, label, row.harness_title.as_deref()) else {
        return Ok(());
    };
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut child = std::process::Command::new(executable)
        .args([
            "mail-inject",
            "--session",
            session,
            "--harness",
            harness,
            "--self-send",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("mail-inject start failed: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("mail-inject stdin unavailable")?;
    stdin
        .write_all(command.as_bytes())
        .map_err(|error| format!("mail-inject write failed: {error}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("mail-inject wait failed: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("mail-inject exited {}", output.status)
        } else {
            detail
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod watch_projection_tests {
        include!("lead_checkin_watch_tests.rs");
    }

    #[test]
    fn cause_rows() {
        let stderr = "fno config: a is not modeled\nfno config: b is not modeled\ngh: API rate limit exceeded for user ID 4994564. (HTTP 403)";
        assert_eq!(
            stderr_cause(stderr),
            "gh: API rate limit exceeded for user ID 4994564. (HTTP 403)"
        );

        let error = "gh api repos/{owner}/{repo}/commits/<sha>/check-runs failed: fno config: x is not modeled\ngh: API rate limit exceeded (HTTP 403)";
        assert_eq!(
            gh_error_cause(error),
            "gh: API rate limit exceeded (HTTP 403)"
        );

        assert_eq!(
            stderr_cause("fno config: first\nfno config: last"),
            "fno config: last"
        );
        assert_eq!(stderr_cause(" \n\t"), "no stderr");

        let cause = "é".repeat(300);
        let result = stderr_cause(&cause);
        assert_eq!(result.chars().count(), 120);
        assert_eq!(result, "é".repeat(120));
    }

    #[test]
    fn count_rows() {
        let first = Value::Array((0..100).map(|n| json!({"number": n})).collect());
        let second = Value::Array((100..107).map(|n| json!({"number": n})).collect());
        assert_eq!(
            open_pr_total(&[first, second, json!({"unexpected": true}), json!([1, 2])]),
            109
        );

        assert_eq!(sanitize_scope_key("fno-x-aaaa epic"), "fno-x-aaaa-epic");
        assert_eq!(sanitize_scope_key("  --x--  "), "x");
        assert_eq!(sanitize_scope_key("///"), "");
    }

    /// A repo fixture whose escalations dir resolves deterministically through
    /// the vault branch: `[project] id` names the project, `[obsidian]`
    /// enabled+vault names the vault root under `home`.
    fn escalations_fixture(dir_name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("fno-checkin-esc-{dir_name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".fno")).unwrap();
        std::fs::write(
            repo.join(".fno/config.toml"),
            "[project]\nid = \"fno\"\n\n[obsidian]\nenabled = true\nvault = \"c3po\"\n",
        )
        .unwrap();
        let dir = base.join("c3po/internal/fno/escalations");
        std::fs::create_dir_all(&dir).unwrap();
        (base, repo, dir)
    }

    /// HOME rides the fixture base for the duration of `f`, restored after.
    fn with_fixture_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let backup = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        let out = f();
        match backup {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        out
    }

    #[test]
    fn escalations_reading_names_overdue_defaults() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (base, repo, dir) = escalations_fixture("overdue");
        with_fixture_home(&base, || {
            let past = "2026-09-01T00:00:00Z";
            std::fs::write(
                dir.join("20260901-0900-a.md"),
                escalation_note("x-1", "money-security", "take-recommended", 2, past),
            )
            .unwrap();
            std::fs::write(
                dir.join("20260901-0901-b.md"),
                escalation_note("x-2", "irreversible", "wait", 1, past),
            )
            .unwrap();
            // Out of scope: counted and printed by neither.
            std::fs::write(
                dir.join("20260901-0902-c.md"),
                escalation_note("x-outside", "money-security", "wait", 1, past),
            )
            .unwrap();
            // A malformed note (take-recommended, no recommend) reads as a
            // wait, never "take option 0".
            let malformed = escalation_note("x-3", "money-security", "take-recommended", 2, past)
                .replace("recommend: 2\n", "");
            std::fs::write(dir.join("20260901-0903-d.md"), malformed).unwrap();
            let folded = Ok(json!({
                "fold": {"nodes": [{"id": "x-1"}, {"id": "x-2"}, {"id": "x-3"}]}
            }));
            let reading = r_escalations(&repo, &folded).unwrap();
            assert_eq!(reading["open"], 3);
            assert_eq!(reading["overdue"], 3);
            let rows = reading["rows"].as_array().unwrap();
            assert!(
                rows.iter().any(|r| r["state"]
                    .as_str()
                    .unwrap()
                    .contains("take option 2 and record it")),
                "{rows:?}"
            );
            assert!(
                rows.iter()
                    .any(|r| r["state"].as_str().unwrap() == "overdue: waits (irreversible)"),
                "{rows:?}"
            );
            assert!(
                rows.iter()
                    .any(|r| r["state"].as_str().unwrap() == "overdue: waits (on_silence wait)"),
                "{rows:?}"
            );
            assert!(
                !rows
                    .iter()
                    .any(|r| r["state"].as_str().unwrap().contains("option 0")),
                "{rows:?}"
            );
            assert!(
                !rows
                    .iter()
                    .any(|r| r["title"].as_str().unwrap().contains("outside")),
                "out-of-scope notes print no row: {rows:?}"
            );
        });
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn escalations_reading_says_unreadable_when_the_path_is_a_file() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (base, repo, dir) = escalations_fixture("unreadable");
        with_fixture_home(&base, || {
            std::fs::remove_dir_all(&dir).unwrap();
            std::fs::write(&dir, "not a directory").unwrap();
            let folded = Ok(json!({"fold": {"nodes": []}}));
            let err = r_escalations(&repo, &folded).unwrap_err();
            assert!(err.starts_with("unreadable ("), "{err}");
        });
    }

    /// One escalation note's frontmatter plus stub sections.
    fn escalation_note(
        node: &str,
        class: &str,
        on_silence: &str,
        recommend: usize,
        deadline: &str,
    ) -> String {
        format!(
            "---\nclass: {class}\nstatus: open\nnode: {node}\nraised_by: lead\nraised_at: 2026-09-01T00:00:00Z\ndeadline: {deadline}\nrecommend: {recommend}\non_silence: {on_silence}\n---\n# t\n\n## What is being decided\nd\n\n## Why it matters now\nw\n\n## Options\n1. A. What happens next: n.\n2. B. What happens next: m.\n\n## Recommendation\nr\n\n## If no answer by the deadline\nx\n"
        )
    }

    #[test]
    fn marker_rows() {
        let doc = "intro\n<!-- fno:user -->\nline one\n<!-- /fno:user -->\ntail\n";
        assert_eq!(extract_user_marker(doc), Some("line one\n".to_string()));

        let doc = "<!-- fno:user -->\nkept\n## Merge order and why (r1)\nnot kept\n";
        assert_eq!(extract_user_marker(doc), Some("kept\n".to_string()));

        let doc = "<!-- fno:status -->\ns\n<!-- fno:user -->\nkept\n<!-- fno:other -->\n";
        assert_eq!(extract_user_marker(doc), Some("kept\n".to_string()));
        assert_eq!(
            extract_user_marker("<!-- fno:user -->\nkept to end"),
            Some("kept to end\n".to_string())
        );
        assert_eq!(extract_user_marker("no marker here"), None);

        assert!(is_user_placeholder(
            "_(write here; the machine reads this every refresh and never edits it)_\n"
        ));
        assert!(!is_user_placeholder("a real note"));
        assert!(!is_user_placeholder(""));
    }

    fn board_payload() -> Value {
        serde_json::from_str(
            r#"{"queues": [
            {"name": "undriven_pr", "status": "ok", "count": 3},
            {"name": "blocked_child", "status": "ok", "rows": [
                {"id": "x-1", "session": "s9", "age_minutes": 90, "reason": "holder dead"}
            ]}
        ]}"#,
        )
        .unwrap()
    }

    fn fold_payload() -> Value {
        serde_json::from_str(
            r#"{"fold": {"status": "ok", "total": 5,
            "counts": {"in_progress": 2, "ready": 1, "done": 2}, "nodes": [
            {"id": "x-2", "status": "in_progress", "worker": "w1", "pr_number": 7,
             "sessions": ["s1", "s2"]}
        ]}, "stuck": {"blocked": [{"id": "x-3", "blocked_by": ["x-1"]}]}}"#,
        )
        .unwrap()
    }

    #[test]
    fn scope_rows() {
        let board = Ok(board_payload());
        let folded = Ok(fold_payload());
        let board_value = r_board(&board, &folded, Ok(7)).unwrap();
        assert_eq!(board_value["blocked"], json!(1));
        assert_eq!(board_value["blocked_on"], json!(["x-3 on x-1"]));
        assert_eq!(board_value["free_claim_no_driver"], json!(3));
        let org = r_org(&folded).unwrap();
        assert_eq!(org["active_nodes"], json!(3));
        assert_eq!(org["total_nodes"], json!(5));
        let rows = org["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["session"], json!("s1"));

        let folded = Ok(json!({"fold": {
            "status": "ok", "total": 15,
            "counts": {"in_progress": 2, "ready": 1, "idea": 5, "deferred": 3, "done": 4},
            "owned_counts": {"in_progress": 1, "idea": 2},
            "nodes": [
                {"id": "x-1", "status": "in_progress", "owned": true},
                {"id": "x-2", "status": "ready", "owned": false},
                {"id": "x-3", "status": "in_progress", "owned": null}
            ]
        }}));
        let org = r_org(&folded).unwrap();
        assert_eq!(org["active_nodes"], json!(3));
        assert_eq!(org["owned_active"], json!(1));
        assert_eq!(org["total_nodes"], json!(15));
        let ids: Vec<&str> = org["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["x-1", "x-3"], "owned false drops; owned null stays");

        let readings = sample_readings(
            json!({"open_prs": 1, "free_claim_no_driver": 0, "blocked": 0, "blocked_on": []}),
            json!({"active_nodes": 3, "owned_active": 1, "total_nodes": 15, "rows": []}),
            cap_ok(),
            workers_none(),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let line = lines
            .iter()
            .find(|l| l.contains("owned active of"))
            .unwrap();
        assert_eq!(line, "x-bbbb: 1 owned active of 3 active, 15 nodes");

        let mut readings = sample_readings(
            board0(),
            json!({"active_nodes": 3, "owned_active": Value::Null, "total_nodes": 15,
                   "owned_reason": "territory: registry unreadable (x)", "rows": []}),
            cap_ok(),
            workers_none(),
        );
        readings
            .iter_mut()
            .find(|reading| reading.name == "territory")
            .unwrap()
            .value = json!([{"scope":"x-aaaa","membership":"unknown","reason":"the graph read returned 0 nodes","rung":2,"mission":"x-aaaa","live":null,"cap":4}]);
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(lines.iter().any(|l| l == "x-bbbb: owned unmeasured (territory: registry unreadable (x)), 3 active, 15 nodes")
            && lines.iter().any(|l| l == "  x-aaaa rung 2 mission x-aaaa live -/4 unreadable (the graph read returned 0 nodes)"));

        let mut prev = prev_row();
        prev["data"].as_object_mut().unwrap().remove("owned_active");
        let data = Map::new();
        let change = derive_change(Some(prev.get("data").unwrap()), &data, "");
        assert_eq!(change, "unmeasured: previous row lacks owned_active");
    }

    /// AC13-HP: the active count is the ACTIVE_STATUSES sum, the owned
    /// headline is the same sum over owned_counts, and a row another team
    /// owns drops off while an unread mark keeps its row.

    /// AC16-HP: the scope line leads with the owned count.

    /// AC14-ERR: a failed owner read renders unmeasured with the reason.

    /// AC15-EDGE: a previous beat row that carries active_nodes but no
    /// owned_active reads unmeasured for one beat, never a fake movement.

    #[test]
    fn epics_rows() {
        // AC3-HP: the lead reads the cap distance on the org reading
        // itself, one line under the active count.
        let org = json!({
            "active_nodes": 3, "total_nodes": 5, "owned_active": 1, "rows": [],
            "epics": [
                {"id": "e-1", "open_children": 16, "full": true},
                {"id": "e-2", "open_children": 3, "full": false}
            ],
            "epic_cap": 15
        });
        let readings = sample_readings(
            board0(),
            org,
            json!({"footprint": "admit", "gate": "admit", "disagree": false,
                   "unparsed_lines": 0, "lanes": ""}),
            workers_none(),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let scope_at = lines
            .iter()
            .position(|l| l.starts_with("x-bbbb: ") && l.contains("active of"))
            .unwrap();
        assert_eq!(lines[scope_at + 1], "epics: e-1 16/15 full, e-2 3/15");

        // AC3-EDGE: `-` per cell and a trailing `(cap unset)`; seven rows
        // name five and count the rest.
        let line = epic_line(&json!({
            "epics": [
                {"id": "e-1", "open_children": 16, "full": true},
                {"id": "e-2", "open_children": 3, "full": false}
            ],
            "epic_cap": Value::Null
        }));
        assert_eq!(line, "epics: e-1 16/-, e-2 3/- (cap unset)");
        let rows: Vec<Value> = (1..=7)
            .map(|i| json!({"id": format!("e-{i}"), "open_children": i, "full": false}))
            .collect();
        let line = epic_line(&json!({"epics": rows, "epic_cap": 15}));
        assert!(line.starts_with("epics: e-1 1/15, e-2 2/15, e-3 3/15, e-4 4/15, e-5 5/15"));
        assert!(line.ends_with("+2 more"));

        // AC3-ERR: a fold that never carried the load reads unmeasured, an
        // empty scope reads none, and a failed org reader prints its own
        // failure instead of an epics line.
        assert_eq!(epic_line(&json!({})), "epics: unmeasured");
        assert_eq!(
            epic_line(&json!({"epics": []})),
            "epics: none in scope holds an open child"
        );
        let mut readings = sample_readings(
            board0(),
            org0(),
            json!({"footprint": "admit", "gate": "admit", "disagree": false,
                   "unparsed_lines": 0, "lanes": ""}),
            workers_none(),
        );
        set_reading(
            &mut readings,
            Reading::failed("org", "scope fold unreadable: no graph".into()),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(
            !lines.iter().any(|l| l.starts_with("epics:")),
            "lines: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("READER FAILED org: ")));
    }

    /// The fixture payloads most render tests share, named once here.
    fn board7() -> Value {
        json!({"open_prs": 7, "free_claim_no_driver": 1, "blocked": 2, "blocked_on": []})
    }
    fn board0() -> Value {
        json!({"open_prs": 0, "free_claim_no_driver": 0, "blocked": 0, "blocked_on": []})
    }
    fn org4() -> Value {
        json!({"active_nodes": 4, "total_nodes": 6, "rows": []})
    }
    fn org0() -> Value {
        json!({"active_nodes": 0, "total_nodes": 0, "rows": []})
    }
    fn cap_ok() -> Value {
        json!({"footprint": "admit", "gate": "admit", "disagree": false, "unparsed_lines": 0})
    }
    fn workers3() -> Value {
        json!({"live_workers": 3, "oldest_worker_seen": "90s w1"})
    }
    fn workers_none() -> Value {
        json!({"live_workers": 0, "oldest_worker_seen": "none"})
    }
    fn workers_empty() -> Value {
        json!({"live_workers": 0, "oldest_worker_seen": ""})
    }

    fn sample_readings(board: Value, org: Value, cap: Value, workers: Value) -> Vec<Reading> {
        vec![
            Reading::took("user_notes", Value::Null),
            Reading::took("board", board),
            Reading::took(
                "blueprint",
                json!({
                    "running": 0,
                    "ceiling": 1,
                    "ceiling_source": "one per lead",
                    "plans_ready": 0,
                    "slots": 4,
                    "starts": [],
                    "skips": [],
                }),
            ),
            Reading::took("escalations", json!({"open": 0, "overdue": 0})),
            Reading::took("blocked_child", json!([{"node": "x-1"}])),
            Reading::took("org", org),
            Reading::took("territory", json!([])),
            Reading::took("capacity", cap),
            Reading::took("workers", workers),
            Reading::took("subagents", json!({"held_idle": 0, "held": []})),
            Reading::took(
                "team",
                json!({"total": 2, "splits": 0, "disagreements": 0, "anomalies": []}),
            ),
            Reading::took(
                "refusal_rate",
                json!({"rate": 0.05, "refused": 5, "total": 100, "window": 100}),
            ),
            Reading::took(
                "wake_meter",
                json!({"machine": 12, "user": 10, "ratio": 1.2, "over": false,
                       "tokens_since": 0, "tokens_session": 0}),
            ),
            Reading::took("drain", json!(9)),
            Reading::took("held", json!({"open": 0, "rows": []})),
            Reading::took("watch_expiry", json!({"rows": []})),
            Reading::took("answered", json!({"rows": []})),
            Reading::took("quiet_workers", json!({"quiet": 0, "read": 0, "rows": []})),
            Reading::took("main_ci", json!("green")),
            Reading::took("control_plane", json!({"attention": []})),
            Reading::took(
                "self_hold",
                json!({"clock_live": false, "clock_until": null, "delivery_policy": null}),
            ),
            Reading::took("parked", json!({"open": 0, "rows": []})),
        ]
    }

    /// Swap in the fixture row that carries `r.name`. A name the fixture
    /// lacks fails the test rather than growing the set.
    fn set_reading(readings: &mut [Reading], r: Reading) {
        let i = readings
            .iter()
            .position(|x| x.name == r.name)
            .unwrap_or_else(|| panic!("fixture has no {} reading", r.name));
        readings[i] = r;
    }

    /// The journaled row carries the same starts and skips the lines print,
    /// and no territory line ever contains `blueprinter` again.
    #[test]
    fn disagree_rows() {
        let readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let bp_line = lines.iter().find(|l| l.starts_with("blueprint:")).unwrap();
        assert_eq!(
            *bp_line,
            "blueprint: running 0 of 1 (one per lead); plans ready 0 / slots 4"
        );
        assert_eq!(
            data.get("blueprint_running"),
            Some(&json!(0)),
            "journal: {data:?}"
        );
        assert_eq!(data.get("blueprint_starts"), Some(&json!([])));
        assert_eq!(data.get("blueprint_skips"), Some(&json!([])));
        let mut territory_seen = false;
        for line in &lines {
            assert!(
                !line.contains("blueprinter"),
                "territory line leaked the blueprinter: {line}"
            );
            if line.starts_with("territory:") {
                territory_seen = true;
            }
        }
        assert!(territory_seen, "territory line missing: {lines:?}");

        let pair = |fp: &str, gate_cpu: &str| {
            r_capacity_pair(
                &json!({"admission": {"verdict": fp}, "unparsed_lines": 0}),
                &json!({
                    "verdict": "refused",
                    "reason": "cpu_share",
                    "rows": [{"name": "cpu-share", "verdict": gate_cpu}]
                }),
            )
            .unwrap()
            .get("disagree")
            .and_then(|d| d.as_bool())
            .unwrap()
        };
        assert!(!pair("admit", "pass"));
        assert!(!pair("refuse", "refuse"));
        assert!(!pair("hold", "hold"));
        assert!(!pair("undecidable", "refuse"));
        assert!(pair("admit", "refuse"));
        assert!(pair("undecidable", "pass"));
        assert!(!pair("admit_degraded", "pass"));
    }

    // ---- check_account_login_with: one test per rule ----

    #[test]
    fn capacity_rows() {
        let capacity = r_capacity_pair(
            &json!({"admission": {"verdict": "admit"}, "unparsed_lines": 0}),
            &json!({
                "verdict": "refused",
                "reason": "lead_share",
                "rows": [{"name": "cpu-share", "verdict": "pass"}]
            }),
        )
        .unwrap();
        assert_eq!(capacity["disagree"], false);
        assert_eq!(capacity["gate_axis"], "lead_share");
        assert_eq!(capacity["gate_cpu"], "pass");
        let readings = sample_readings(board0(), org0(), capacity, workers_none());
        let data = build_data(&readings, "x-bbbb");
        let line = render_lines("x-bbbb", &readings, &data, &None, "", "no change")
            .into_iter()
            .find(|line| line.starts_with("capacity:"))
            .unwrap();
        assert!(
            line.contains("gate refused on lead_share, cpu pass"),
            "line: {line}"
        );
        assert!(!line.contains("DISAGREE"), "line: {line}");

        let capacity = r_capacity_pair(
            &json!({"admission": {"verdict": "admit"}, "unparsed_lines": 0}),
            &json!({
                "verdict": "refused",
                "reason": "cpu_share_undecidable",
                "rows": [{"name": "cpu-share", "verdict": "refuse"}]
            }),
        )
        .unwrap();
        assert_eq!(capacity["disagree"], true);
        let readings = sample_readings(board0(), org0(), capacity, workers_none());
        let data = build_data(&readings, "x-bbbb");
        let line = render_lines("x-bbbb", &readings, &data, &None, "", "no change")
            .into_iter()
            .find(|line| line.starts_with("capacity:"))
            .unwrap();
        assert!(line.contains("DISAGREE"), "line: {line}");
        assert!(
            line.contains("gate refused on cpu_share_undecidable, cpu refuse"),
            "line: {line}"
        );

        let capacity = r_capacity_pair(
            &json!({"admission": {"verdict": "admit"}, "unparsed_lines": 0}),
            &json!({
                "verdict": "refused",
                "reason": "ram_floor",
                "rows": [{"name": "ram", "verdict": "refuse"}]
            }),
        )
        .unwrap();
        assert!(capacity["gate_cpu"].is_null());
        assert_eq!(capacity["disagree"], false);
        let readings = sample_readings(board0(), org0(), capacity, workers_none());
        let data = build_data(&readings, "x-bbbb");
        let line = render_lines("x-bbbb", &readings, &data, &None, "", "no change")
            .into_iter()
            .find(|line| line.starts_with("capacity:"))
            .unwrap();
        assert!(line.contains("cpu -"), "line: {line}");
        assert!(!line.contains("DISAGREE"), "line: {line}");

        let capacity = r_capacity_pair(
            &json!({"admission": {"verdict": "admit"}, "unparsed_lines": 0}),
            &json!({
                "verdict": "refused",
                "lanes": {
                    "zai": {"cap": 10, "live": 8, "quota": "closed"},
                    "openai": {"cap": null, "live": 1, "quota": "unmeasured"}
                }
            }),
        )
        .unwrap();
        assert_eq!(capacity["lanes"], "openai 1/- unmeasured, zai 8/10 closed");
        let readings = sample_readings(board0(), org0(), capacity.clone(), workers_none());
        let data = build_data(&readings, "x-bbbb");
        assert_eq!(
            data.get("capacity_lanes"),
            Some(&json!("openai 1/- unmeasured, zai 8/10 closed"))
        );
        let line = render_lines("x-bbbb", &readings, &data, &None, "", "no change")
            .into_iter()
            .find(|line| line.starts_with("capacity:"))
            .unwrap();
        assert!(
            line.ends_with("| lanes openai 1/- unmeasured, zai 8/10 closed"),
            "line: {line}"
        );

        let unreadable = r_capacity_pair(
            &json!({"admission": {"verdict": "admit"}, "unparsed_lines": 0}),
            &json!({"verdict": "accepted"}),
        )
        .unwrap();
        assert_eq!(unreadable["lanes"], "lanes unreadable");
        let unread_readings = sample_readings(board0(), org0(), unreadable, workers_none());
        let unread_data = build_data(&unread_readings, "x-bbbb");
        let unread_line = render_lines(
            "x-bbbb",
            &unread_readings,
            &unread_data,
            &None,
            "",
            "no change",
        )
        .into_iter()
        .find(|line| line.starts_with("capacity:"))
        .unwrap();
        assert!(unread_line.ends_with("| lanes lanes unreadable"));

        let capacity_line = |unparsed: i64| {
            let capacity = r_capacity_pair(
                &json!({"admission": {"verdict": "admit"}, "unparsed_lines": unparsed}),
                &json!({"verdict": "accepted"}),
            )
            .unwrap();
            let readings = sample_readings(board0(), org0(), capacity, workers_none());
            let data = build_data(&readings, "x-bbbb");
            render_lines("x-bbbb", &readings, &data, &None, "", "no change")
                .into_iter()
                .find(|line| line.starts_with("capacity:"))
                .unwrap()
        };
        let floored = capacity_line(3);
        assert!(
            floored.contains("floor: 3 ps row(s) unparsed"),
            "names the count: {floored}"
        );
        assert!(
            floored.contains("their CPU missing"),
            "names the direction: {floored}"
        );
        assert!(
            floored.contains("admits permissively"),
            "names the bias: {floored}"
        );
        assert!(
            floored.contains("fno doctor footprint --json"),
            "names a command to run: {floored}"
        );
        let clean = capacity_line(0);
        assert!(
            !clean.contains("floor:") && !clean.contains("unparsed"),
            "zero stays unchanged: {clean}"
        );
    }

    // The subagents reading: the workers line carries the fleet's active
    // count, the held line names the finished agents this session still
    // holds with their TaskStop remedy, and held rows raise attention.
    #[test]
    fn workers_rows() {
        let mut readings = sample_readings(
            board7(),
            org4(),
            cap_ok(),
            json!({"live_workers": 3, "oldest_worker_seen": "90s w1", "live_subagents": 3}),
        );
        set_reading(
            &mut readings,
            Reading::took(
                "subagents",
                json!({
                    "held_idle": 2,
                    "held": [
                        {"id": "a1", "name": null, "status": "completed", "idle_secs": 7200},
                        {"id": "a2", "name": "bp-x", "status": "failed", "idle_secs": 3600}
                    ]
                }),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert!(
            change.starts_with("attention:"),
            "held rows raise attention: {change}"
        );
        assert!(
            change.contains("2 finished subagents held unstopped"),
            "change: {change}"
        );
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", &change);
        let workers_line = lines.iter().find(|l| l.starts_with("workers:")).unwrap();
        assert!(
            workers_line.contains("subagents active 3"),
            "line: {workers_line}"
        );
        let held_line = lines.iter().find(|l| l.starts_with("subagents:")).unwrap();
        assert!(
            held_line.contains("holds 2 finished and unstopped"),
            "line: {held_line}"
        );
        assert!(held_line.contains("TaskStop a1"), "line: {held_line}");
        assert!(held_line.contains("TaskStop bp-x"), "line: {held_line}");

        let mut readings = sample_readings(
            board7(),
            org4(),
            cap_ok(),
            json!({"live_workers": 3, "oldest_worker_seen": "90s w1", "live_subagents": 3}),
        );
        set_reading(
            &mut readings,
            Reading::failed(
                "subagents",
                "the wake and refusal readers need a claude transcript; \
                 this session's harness is not claude"
                    .into(),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        assert_eq!(data.get("coverage"), Some(&json!(21)), "21 of 22 ok");
        assert_eq!(data.get("idle_subagents"), None);
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("READER FAILED subagents:")),
            "lines: {lines:?}"
        );

        let readings = sample_readings(board7(), org4(), cap_ok(), workers3());
        let data = build_data(&readings, "x-bbbb");
        assert_eq!(data.get("live_subagents"), Some(&Value::Null));
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let workers_line = lines.iter().find(|l| l.starts_with("workers:")).unwrap();
        assert!(
            workers_line.contains("subagents active -"),
            "line: {workers_line}"
        );
    }

    // AC5: a non-claude harness fails the reading, the beat prints
    // READER FAILED subagents:, and coverage counts it as failed.

    // AC6-EDGE: a top payload with no subagents key renders `-`, never 0.

    // AC4: an over-ceiling wake ratio prints the OVER suffix and journals an
    // attention item, so an over beat is never journalled as a quiet one.
    #[test]
    fn wake_rows() {
        let mut readings = sample_readings(board7(), org4(), cap_ok(), workers3());
        set_reading(
            &mut readings,
            Reading::took(
                "wake_meter",
                json!({"machine": 287, "user": 44, "ratio": 6.5, "over": true,
                       "tokens_since": 318429, "tokens_session": 11567215}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert!(change.starts_with("attention:"), "change: {change}");
        assert!(change.contains("wake ratio"));
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", &change);
        let line = lines.iter().find(|l| l.starts_with("wake_ratio:")).unwrap();
        assert_eq!(
            line,
            "wake_ratio: 287 machine / 44 user wakes = 6.5 to 1 - OVER 3 to 1"
        );

        let mut readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
        set_reading(
            &mut readings,
            Reading::failed(
                "wake_meter",
                "transcript unreadable: no such file".to_string(),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(
            lines
                .iter()
                .any(|l| l == "READER FAILED wake_meter: transcript unreadable: no such file"),
            "lines: {lines:?}"
        );
        assert!(!lines.iter().any(|l| l.starts_with("wake_ratio:")));
        assert!(!lines.iter().any(|l| l.starts_with("subagent_tokens:")));

        let mut readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
        set_reading(
            &mut readings,
            Reading::took(
                "wake_meter",
                json!({"machine": 5, "user": 0, "ratio": null, "over": true,
                       "tokens_since": 0, "tokens_session": 0}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert!(change.contains("wake ratio n/a"), "change: {change}");
        assert!(!change.contains("0.0 to 1"), "change: {change}");
    }

    // AC5: a failed wake_meter reading prints the READER FAILED line, names
    // itself in readers_failed, and no wake_ratio or subagent_tokens line
    // prints.

    // A zero-user over beat journals n/a, never a 0.0 ratio that contradicts
    // the printed n/a line.

    // AC2: the handoff signal reads the true beat-to-beat direction. Two
    // consecutive rises trip it; one rise, flat, or falling does not; and the
    // baseline advances with every measured beat, so beats whose row never
    // journaled can never pin the comparison to a stale pair.
    #[test]
    fn refusal_rows() {
        // Two rises: 0.05 -> 0.10 -> 0.20 trips the signal.
        let mut data: Map<String, Value> = Map::new();
        data.insert("refusal_rate".into(), json!(0.20));
        mark_refusal_rate_trend(&mut data, Some(0.10), Some(0.05));
        assert_eq!(data.get("refusal_rate_rising"), Some(&json!(true)));
        assert_eq!(
            data.get("refusal_rate_trend_unmeasured"),
            Some(&json!(false))
        );

        // One rise only: 0.10 -> 0.10 -> 0.20 (flat, then up).
        mark_refusal_rate_trend(&mut data, Some(0.10), Some(0.10));
        assert_eq!(data.get("refusal_rate_rising"), Some(&json!(false)));

        // Falling into the current beat: 0.05 -> 0.30 -> 0.20.
        mark_refusal_rate_trend(&mut data, Some(0.30), Some(0.05));
        assert_eq!(data.get("refusal_rate_rising"), Some(&json!(false)));

        // Missing history reads unmeasured, never a false positive.
        mark_refusal_rate_trend(&mut data, None, None);
        assert_eq!(data.get("refusal_rate_rising"), Some(&json!(false)));
        assert_eq!(
            data.get("refusal_rate_trend_unmeasured"),
            Some(&json!(true))
        );

        // The filed defect: a falling series of unjournalled beats. Each
        // beat's record advances the baseline, so the label follows the
        // true direction instead of the same stale pair.
        let dir = tempfile::tempdir().unwrap();
        let trend = dir.path();
        for (n, current) in [0.175, 0.165, 0.150].iter().enumerate() {
            let (p1, p2) = crate::refusal_trend::priors(trend, "x-bbbb");
            let mut data: Map<String, Value> = Map::new();
            data.insert("refusal_rate".into(), json!(current));
            mark_refusal_rate_trend(&mut data, p1, p2);
            assert_eq!(
                data.get("refusal_rate_rising"),
                Some(&json!(false)),
                "beat {n}: a falling series must not read RISING"
            );
            crate::refusal_trend::record(
                trend,
                "x-bbbb",
                &format!("2026-09-15T10:0{n}:00Z"),
                *current,
            );
        }

        let readings = sample_readings(board7(), org4(), cap_ok(), workers3());
        let mut data = build_data(&readings, "x-bbbb");
        assert_eq!(data.get("refusal_rate"), Some(&json!(0.05)));
        data.insert("refusal_rate_rising".into(), json!(true));
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let line = lines
            .iter()
            .find(|l| l.starts_with("refusal_rate:"))
            .unwrap();
        assert_eq!(
            line,
            "refusal_rate: 5.0% (5/100 last 100 calls) - RISING (handoff signal)"
        );

        // A missing prior pair is stated on the line, never silently blank.
        let mut data = build_data(&readings, "x-bbbb");
        data.insert("refusal_rate_trend_unmeasured".into(), json!(true));
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let line = lines
            .iter()
            .find(|l| l.starts_with("refusal_rate:"))
            .unwrap();
        assert_eq!(
            line,
            "refusal_rate: 5.0% (5/100 last 100 calls) - UNMEASURED (needs two prior beats)"
        );

        let mut data: Map<String, Value> = Map::new();
        data.insert("refusal_rate_rising".into(), json!(true));
        let change = derive_change(None, &data, "");
        assert!(
            change.starts_with("attention: refusal rate rising"),
            "change: {change}"
        );
    }

    // AC1+AC2: the printed line carries the real refused/total/window counts
    // and exactly one trend verdict - RISING on two consecutive rises,
    // UNMEASURED on a missing prior pair - and the same rate lands in the
    // journaled data.

    // A rising refusal rate outranks silence the same way control-plane
    // attention does: it must never journal as "no change".

    #[test]
    fn park_rows() {
        let mut readings = sample_readings(
            json!({"open_prs": 2, "free_claim_no_driver": 0, "blocked": 0, "blocked_on": []}),
            json!({"active_nodes": 1, "total_nodes": 2, "rows": []}),
            cap_ok(),
            json!({"live_workers": 1, "oldest_worker_seen": "30s w1"}),
        );
        set_reading(
            &mut readings,
            Reading::took(
                "parked",
                json!({"open": 2, "rows": [
                    {"key": "owner/repo#101", "node": "x-aa",
                     "reason_detail": "failed; checks are red", "age_hours": 2},
                    {"key": "owner/repo#2078", "node": "x-bb",
                     "reason_detail": "failed; checks are red", "age_hours": 5},
                ]}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let unpark_rows: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("pr-park unpark"))
            .collect();
        assert_eq!(unpark_rows.len(), 2, "lines: {lines:?}");
        assert!(unpark_rows[0].contains("owner/repo#101"));
        assert!(unpark_rows[0].contains("checks are red"));

        let mut readings = sample_readings(
            json!({"open_prs": 2, "free_claim_no_driver": 0, "blocked": 0, "blocked_on": []}),
            json!({"active_nodes": 1, "total_nodes": 2, "rows": []}),
            cap_ok(),
            json!({"live_workers": 1, "oldest_worker_seen": "30s w1"}),
        );
        set_reading(
            &mut readings,
            Reading::took("parked", json!({"open": 0, "rows": []})),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(
            lines.iter().any(|l| l == "parked: none"),
            "lines: {lines:?}"
        );
    }

    fn journal(dir: &Path, rows: &[Value]) -> PathBuf {
        let path = dir.join("events.jsonl");
        let mut f = std::fs::File::create(path).unwrap();
        for r in rows {
            writeln!(f, "{r}").unwrap();
        }
        dir.join("events.jsonl")
    }

    #[test]
    fn answer_rows() {
        let mut readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
        set_reading(
            &mut readings,
            Reading::took(
                "answered",
                json!({"rows": [
                    {"node": "x-1", "question_id": "q-1", "answer": "keep the lane",
                     "ts": "2026-09-10T12:00:00Z", "epoch": 0},
                ]}),
            ),
        );
        set_reading(
            &mut readings,
            Reading::took(
                "quiet_workers",
                json!({"quiet": 1, "read": 1, "rows": [
                    {"worker": "t-x-1-glm", "node": "x-1",
                     "line": "RESULT: BLOCKED need a ruling"},
                ]}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(
            lines
                .iter()
                .any(|l| l == "answered: 1 user decision(s) in scope"),
            "lines: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("x-1 (q-1): keep the lane")));
        assert!(lines.iter().any(|l| l == "quiet: 1 worker(s) in scope"));
        assert!(lines
            .iter()
            .any(|l| l.contains("t-x-1-glm (x-1): RESULT: BLOCKED need a ruling")));
        // A failed read keeps its line and says so; it never blanks.
        set_reading(
            &mut readings,
            Reading::failed("quiet_workers", "peek exited 13".into()),
        );
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(
            lines
                .iter()
                .any(|l| l == "READER FAILED quiet_workers: peek exited 13"),
            "lines: {lines:?}"
        );

        let mut readings = sample_readings(board0(), org0(), cap_ok(), workers_empty());
        set_reading(
            &mut readings,
            Reading::took(
                "held",
                json!({"open": 3, "rows": [
                    {"node": "x-1", "question_id": "q-1", "question": "pick", "ts": "2026-09-10T12:00:00Z", "epoch": 0},
                    {"node": "x-2", "question_id": "q-2", "question": "pick", "ts": "2026-09-10T12:00:00Z", "epoch": 0},
                    {"node": null, "question_id": "q-3", "question": "pick", "ts": "2026-09-10T12:00:00Z", "epoch": 0}
                ]}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        assert_eq!(data.get("held_open"), Some(&json!(3)));
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        let summary: Vec<&String> = lines.iter().filter(|l| l.starts_with("held: ")).collect();
        assert_eq!(summary.len(), 1, "lines: {lines:?}");
        assert!(
            summary[0].contains("3 question(s) for this team"),
            "lines: {lines:?}"
        );
        let verbs: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("fno backlog decide"))
            .collect();
        assert_eq!(verbs.len(), 2, "each row names the decide verb");
        let clears: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("fno inbox outstanding clear"))
            .collect();
        assert_eq!(clears.len(), 1, "the nodeless row names the clear verb");
        assert!(
            clears[0].contains("the user answers it on the question board"),
            "lines: {lines:?}"
        );
        // An absent held read keeps its line and says none; coverage counts it.
        readings.retain(|r| r.name != "held");
        let data = build_data(&readings, "x-bbbb");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(lines.iter().any(|l| l == "held: none"), "lines: {lines:?}");
        assert!(lines.iter().any(|l| l == "coverage: 21 of 21 readings ok"));
    }

    fn prev_row() -> Value {
        json!({"ts": "2026-09-10T12:00:00Z", "type": "lead_checkin", "source": "loop",
            "data": {"scope": "x-bbbb", "change": "no change", "open_prs": 9,
                     "free_claim_no_driver": 1, "blocked": 2,
                     "escalations_open": 0, "escalations_overdue": 0,
                     "owned_active": 2, "live_workers": 3, "undelivered": 9,
                     "held_open": 0, "blueprint_running": 0, "blueprint_ceiling": 1}})
    }

    #[test]
    fn diff_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal(dir.path(), &[prev_row()]);
        let ctx = Ctx {
            scope: "x-bbbb".into(),
            level: Some(1),
            events_paths: vec![path],
            graph: PathBuf::from("nope.json"),
            cwd: dir.path().to_path_buf(),
            handoffs_dir: dir.path().to_path_buf(),
            faqs_dir: None,
            board_state: None,
            emit_path: None,
            emit: false,
        };
        let (previous, err) = previous_row(&ctx, None);
        assert!(err.is_empty());
        let readings = sample_readings(
            board7(),
            json!({"active_nodes": 4, "total_nodes": 6, "owned_active": 2, "rows": []}),
            cap_ok(),
            workers3(),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(previous.as_ref().and_then(|p| p.get("data")), &data, "");
        assert_eq!(change, "moved: open_prs 9 -> 7");
        let lines = render_lines("x-bbbb", &readings, &data, &previous, "", &change);
        let diff_line = lines
            .iter()
            .find(|l| l.starts_with("vs last beat (2026-09-10T12:00:00Z)"))
            .unwrap();
        assert!(diff_line.contains("open_prs 9 -> 7"), "line: {diff_line}");

        let dir = tempfile::tempdir().unwrap();
        let path = journal(dir.path(), &[prev_row()]);
        let ctx = Ctx {
            scope: "x-bbbb".into(),
            level: Some(1),
            events_paths: vec![path],
            graph: PathBuf::from("nope.json"),
            cwd: dir.path().to_path_buf(),
            handoffs_dir: dir.path().to_path_buf(),
            faqs_dir: None,
            board_state: None,
            emit_path: None,
            emit: false,
        };
        let (previous, err) = previous_row(&ctx, None);
        assert!(err.is_empty());
        let mut readings = sample_readings(
            json!({"open_prs": 9, "free_claim_no_driver": 1, "blocked": 2, "blocked_on": []}),
            json!({"active_nodes": 4, "total_nodes": 6, "owned_active": 2, "rows": []}),
            cap_ok(),
            workers3(),
        );
        set_reading(
            &mut readings,
            Reading::took(
                "control_plane",
                json!({"attention": ["pr_watch_merge FAIL timeout for 2000s"]}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(previous.as_ref().and_then(|p| p.get("data")), &data, "");
        assert!(
            change.starts_with("attention: pr_watch_merge FAIL timeout for 2000s"),
            "{change}"
        );
        let lines = render_lines("x-bbbb", &readings, &data, &previous, "", &change);
        assert!(lines.iter().any(|l| l == "control plane:"), "{lines:?}");
        assert!(lines
            .iter()
            .any(|l| l == "  pr_watch_merge FAIL timeout for 2000s"));
        assert_eq!(
            data.get("control_plane_attention"),
            Some(&json!(["pr_watch_merge FAIL timeout for 2000s"]))
        );

        // A count that also moved still names itself, after the attention.
        set_reading(&mut readings, Reading::took("board", board7()));
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(previous.as_ref().and_then(|p| p.get("data")), &data, "");
        assert_eq!(
            change,
            "attention: pr_watch_merge FAIL timeout for 2000s; moved: open_prs 9 -> 7"
        );

        let mut readings = sample_readings(
            json!({"open_prs": 9, "free_claim_no_driver": 1, "blocked": 2, "blocked_on": []}),
            json!({"active_nodes": 4, "total_nodes": 6, "owned_active": 2, "rows": []}),
            cap_ok(),
            workers3(),
        );
        set_reading(
            &mut readings,
            Reading::failed("control_plane", "journals unreadable".into()),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert_eq!(
            change,
            "no numeric movement; readings failed: control_plane"
        );
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", &change);
        assert!(lines
            .iter()
            .any(|l| l == "READER FAILED control_plane: journals unreadable"));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("coverage: 21 of 22 readings ok")));

        let mut readings = sample_readings(board7(), org4(), cap_ok(), workers3());
        let data = build_data(&readings, "x-bbbb");
        assert_eq!(
            derive_change(None, &data, ""),
            "first canonical beat for this scope"
        );
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", "no change");
        assert!(lines
            .iter()
            .any(|l| l == "self_hold: clock inactive; delivery_policy none"));
        assert!(lines.iter().any(|l| l == "control plane: ok"));

        set_reading(
            &mut readings,
            Reading::took(
                "self_hold",
                json!({"clock_live": false, "clock_until": null, "delivery_policy": "bus-only"}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert_eq!(change, "attention: DND on");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", &change);
        assert!(lines
            .iter()
            .any(|l| l == "self_hold: clock inactive; delivery_policy bus-only"));
        assert!(lines.iter().any(|l| l == "attention: DND on"));

        set_reading(
            &mut readings,
            Reading::took(
                "self_hold",
                json!({"clock_live": true, "clock_until": "2030-01-01T00:00:00Z", "delivery_policy": null}),
            ),
        );
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert_eq!(change, "attention: DND on");
        let lines = render_lines("x-bbbb", &readings, &data, &None, "", &change);
        assert!(lines.iter().any(
            |l| l == "self_hold: clock active until 2030-01-01T00:00:00Z; delivery_policy none"
        ));

        // A stale carried skill body journals attention, never no change.
        readings.push(Reading::took(
            "skill_drift",
            json!({
                "compacted_at": "2026-09-30T01:35:00Z",
                "carried": 1,
                "stale": [{"name": "fno:lead", "file": "/tmp/skills/lead/SKILL.md", "reason": "text drift"}],
            }),
        ));
        let data = build_data(&readings, "x-bbbb");
        let change = derive_change(None, &data, "");
        assert!(change.starts_with("attention:"), "change: {change}");
        assert!(
            change.contains("skill text stale since compaction: fno:lead"),
            "change: {change}"
        );

        let readings = sample_readings(board7(), org4(), cap_ok(), workers3());
        let data = build_data(&readings, "x-bbbb");
        let hand = json!({"ts": "2026-09-15T13:40:38Z", "type": "lead_checkin", "source": "hand",
            "data": {"scope": "x-bbbb", "change": "resumed the team"}});
        let change = derive_change(Some(hand.get("data").unwrap()), &data, "");
        assert!(
            change.starts_with("unmeasured: previous row lacks"),
            "change: {change}"
        );
        assert!(!change.contains("no change"), "change: {change}");
        let lines = render_lines("x-bbbb", &readings, &data, &Some(hand), "", &change);
        let beat = lines
            .iter()
            .find(|l| l.starts_with("vs last beat"))
            .unwrap();
        assert!(
            beat.contains("unmeasured (previous row lacks"),
            "line: {beat}"
        );
    }

    // AC6-HP: a 30-minute arm FAIL is attention, and a moved count still
    // reports itself inside the attention change.

    // AC6-ERR: a failed control_plane reading prints its own line, counts
    // against coverage, and blocks the "no change" verdict.

    // The self-hold line exposes both inputs, and either one raises attention.

    #[test]
    fn faq_scope_line_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lead-s-1234.md");
        std::fs::write(&path, "---\ncreated: t\nscope: x-bbbb\n---\n\n# q\n").unwrap();
        std::fs::write(dir.path().join("lead-other.md"), "---\nscope: other\n---\n").unwrap();
        let entries = faq_entries_for_scope(dir.path(), "x-bbbb");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].starts_with("---"));
    }

    fn emit_ctx(dir: &tempfile::TempDir, scope: &str) -> (Ctx, PathBuf) {
        let path = dir.path().join("events.jsonl");
        (
            Ctx {
                scope: scope.into(),
                level: Some(1),
                events_paths: vec![path.clone()],
                graph: PathBuf::from("nope.json"),
                cwd: dir.path().to_path_buf(),
                handoffs_dir: dir.path().to_path_buf(),
                faqs_dir: None,
                board_state: None,
                emit_path: Some(path.clone()),
                emit: true,
            },
            path,
        )
    }

    #[test]
    fn emit_rows() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, path) = emit_ctx(&dir, "x-bbbb");
        let data = json!({"scope": "x-bbbb", "change": "beat"});
        assert!(emit_row(
            ctx.emit_path.as_ref().unwrap(),
            "loop",
            data.as_object().unwrap()
        ));
        let rows = crate::events::committed_journal_text(&path);
        assert_eq!(rows.lines().count(), 1);
        assert!(rows.contains("lead_checkin"));
        assert!(rows.contains("\"source\":\"loop\""), "rows: {rows}");

        assert_eq!(finish_checkin(true, false, None), 3);

        assert_eq!(finish_checkin(false, false, None), 0);

        assert_eq!(
            finish_checkin(
                true,
                true,
                Some(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            ),
            0
        );

        assert_eq!(
            finish_checkin(
                true,
                false,
                Some(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            ),
            3
        );

        let dir = tempfile::tempdir().unwrap();
        let (ctx, path) = emit_ctx(&dir, "x-cccc ready no build, idea");
        let data = json!({"scope": "x-cccc ready no build, idea", "change": "beat"});
        assert!(!emit_row(
            ctx.emit_path.as_ref().unwrap(),
            "loop",
            data.as_object().unwrap()
        ));
        assert!(
            !path.exists()
                || crate::events::committed_journal_text(&path)
                    .trim()
                    .is_empty(),
            "the corrupted-scope row must not reach the journal"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let data = json!({"scope": "x-bbbb", "change": "beat"});
        assert!(emit_row(&path, "loop", data.as_object().unwrap()));
        let rows = crate::events::committed_journal_text(&path);
        assert_eq!(rows.lines().count(), 1);
        assert!(rows.contains("\"source\":\"loop\""), "rows: {rows}");

        // An oversized payload journals the meta-event, never a raw row.
        let huge = json!({"scope": "x-bbbb", "change": "x".repeat(70_000)});
        assert!(emit_row(&path, "loop", huge.as_object().unwrap()));
        let rows = crate::events::committed_journal_text(&path);
        assert_eq!(rows.lines().count(), 2, "rows: {rows}");
        assert!(rows.contains("event_payload_too_large"), "rows: {rows}");
        assert!(
            rows.contains("\"intended_kind\":\"lead_checkin\""),
            "rows: {rows}"
        );

        let mut data = Map::new();
        let change = finish_change(
            "moved: open_prs 9 -> 7".into(),
            Some("dispatched two workers"),
            &mut data,
        );
        assert_eq!(change, "dispatched two workers");
        assert_eq!(data.get("change"), Some(&json!("dispatched two workers")));
        assert_eq!(data.get("diff"), Some(&json!("moved: open_prs 9 -> 7")));

        // A blank sentence reads as none: the derived text fills both keys.
        let mut data = Map::new();
        let change = finish_change("no change".into(), Some("   "), &mut data);
        assert_eq!(change, "no change");
        assert_eq!(data.get("change"), Some(&json!("no change")));
        assert_eq!(data.get("diff"), Some(&json!("no change")));

        let dir = tempfile::tempdir().unwrap();
        let rows = [
            json!({"ts": "2026-09-15T10:00:00Z", "type": "lead_checkin", "source": "loop",
                   "data": {"scope": "x-bbbb", "change": "beat", "open_prs": 9}}),
            json!({"ts": "2026-09-15T10:05:00Z", "type": "lead_checkin", "source": "hook",
                   "data": {"scope": "x-bbbb", "change": "missed beat", "open_prs": 8}}),
            json!({"ts": "2026-09-15T10:10:00Z", "type": "lead_checkin", "source": "test",
                   "data": {"scope": "x-bbbb", "change": "hand row", "open_prs": 7}}),
        ];
        let path = journal(dir.path(), &rows);
        let ctx = Ctx {
            scope: "x-bbbb".into(),
            level: Some(1),
            events_paths: vec![path],
            graph: PathBuf::from("nope.json"),
            cwd: dir.path().to_path_buf(),
            handoffs_dir: dir.path().to_path_buf(),
            faqs_dir: None,
            board_state: None,
            emit_path: None,
            emit: false,
        };
        let (previous, err) = previous_row(&ctx, None);
        assert!(err.is_empty(), "err: {err}");
        let previous = previous.expect("the newest loop row is the baseline");
        assert_eq!(s_str(&previous, "source"), Some("loop"));
        assert_eq!(previous["data"]["open_prs"], 9);
    }

    fn pause_row(arm: &str) -> crate::tick_ledger::ArmStatus {
        let mut r = crate::tick_ledger::ArmStatus {
            arm: arm.to_string(),
            scheduler: Some(crate::tick_ledger::SCHED_LAUNCHD.to_string()),
            last_ts: Some("2026-09-17T22:30:00Z".to_string()),
            age_s: Some(600),
            acted: Some(0),
            skip_reason: None,
            detail: None,
            interval_s: 900,
            producer_evidence: crate::tick_ledger::ProducerEvidence::Observed,
            stale: false,
            failing: false,
            failing_for_s: None,
            cause: None,
            line: String::new(),
            repair: None,
            heal: None,
            upstream: None,
            arm_key: None,
            arm_value: None,
            reader: None,
            starved: false,
            retries: Vec::new(),
        };
        r.cause = Some("fleet_stop".to_string());
        r.line = format!(
            "{} cause=fleet_stop (fleet incident stopped at generation 5: two cargo runs; \
             held on purpose; wait for the breaker to clear)",
            crate::tick_ledger::render_row(&r)
        );
        r
    }

    // AC9-HP: a paused tier leads the attention list with one breaker
    // summary, and no line prescribes a refresh.
    #[test]
    fn pause_rows() {
        use crate::loops_pause::DispatchPause;
        let rows: Vec<crate::tick_ledger::ArmStatus> =
            ["lead_wake", "watchdog", "pr_watch_merge", "notify_watch"]
                .iter()
                .map(|a| pause_row(a))
                .collect();
        let trace = crate::tick_ledger::TickTrace {
            pause: Some(DispatchPause::FleetIncident {
                generation: 5,
                reason: "two cargo runs".to_string(),
                holds: vec!["spawns".to_string(), "tests".to_string()],
            }),
            ..crate::tick_ledger::TickTrace::default()
        };
        let out = control_plane_attention(&rows, &trace, &[], 1800);
        assert_eq!(out.len(), 1, "lines: {out:?}");
        assert!(out[0].contains("generation 5"), "line: {}", out[0]);
        assert!(
            out[0].contains("fno agents incident status"),
            "line: {}",
            out[0]
        );
        assert!(
            out[0].starts_with("4 arms paused on purpose:"),
            "line: {}",
            out[0]
        );
        // The reach is read, not claimed: holds from the record, and merges
        // named as proceeding while they are not in the hold list.
        assert!(
            out[0].contains("the breaker holds spawns, tests"),
            "line: {}",
            out[0]
        );
        assert!(
            out[0].contains("merges and loops proceed"),
            "line: {}",
            out[0]
        );
        assert!(
            out.iter()
                .all(|l| !l.contains("merges and dispatch are held")),
            "the false claim must be gone: lines {out:?}"
        );
        assert!(
            out.iter().all(|l| !l.contains("tick_overdue")),
            "lines: {out:?}"
        );
        assert!(
            out.iter().all(|l| !l.contains("pr watch refresh")),
            "lines: {out:?}"
        );

        let rows: Vec<crate::tick_ledger::ArmStatus> = ["lead_wake", "watchdog"]
            .iter()
            .map(|a| pause_row(a))
            .collect();
        let trace = crate::tick_ledger::TickTrace {
            pause: Some(DispatchPause::Manual {
                state: "paused".to_string(),
                detail: "loops paused by op".to_string(),
            }),
            ..crate::tick_ledger::TickTrace::default()
        };
        let out = control_plane_attention(&rows, &trace, &[], 1800);
        assert_eq!(out.len(), 1, "lines: {out:?}");
        assert!(
            out[0].contains("loop dispatch is held; merges proceed"),
            "line: {}",
            out[0]
        );
        assert!(out[0].contains("fno do loops status"), "line: {}", out[0]);

        let mut kw = crate::tick_ledger::ArmStatus {
            arm: "lead_wake".to_string(),
            scheduler: Some(crate::tick_ledger::SCHED_LAUNCHD.to_string()),
            last_ts: Some("2026-09-17T22:30:00Z".to_string()),
            age_s: Some(2400),
            acted: Some(0),
            skip_reason: None,
            detail: None,
            interval_s: 900,
            producer_evidence: crate::tick_ledger::ProducerEvidence::Observed,
            stale: true,
            failing: false,
            failing_for_s: None,
            cause: None,
            line: String::new(),
            repair: None,
            heal: None,
            upstream: None,
            arm_key: None,
            arm_value: None,
            reader: None,
            starved: false,
            retries: Vec::new(),
        };
        kw.cause = Some("tick_overdue".to_string());
        kw.line = format!(
            "{} cause=tick_overdue (no tick stamp inside 2x interval)",
            crate::tick_ledger::render_row(&kw)
        );
        let rows = vec![kw];
        let out =
            control_plane_attention(&rows, &crate::tick_ledger::TickTrace::default(), &[], 1800);
        assert_eq!(out.len(), 1, "lines: {out:?}");
        assert!(out[0].starts_with("lead_wake"), "line: {}", out[0]);
        assert!(out[0].contains("tick_overdue"), "line: {}", out[0]);
    }

    // AC-EDGE: a manual loops pause names what it holds, and the tail
    // never claims merges are held.

    // AC10-EDGE: no pause, the output is today's: the row lines only.

    #[test]
    fn team_rows() {
        let splits = crate::team_split::TeamSplits {
            double_ruled: vec![crate::team_split::ScopeSplit {
                scope: "shared".into(),
                holders: vec!["lead-a".into(), "lead-b".into()],
            }],
            stale: vec![crate::team_split::StaleCrown {
                row: "lead-dead".into(),
                scope: "shared".into(),
                stored_status: "orphaned".into(),
            }],
        };
        let (double_ruled, stale_teamed, err, ruled, stale) =
            team_split_fields(Ok(splits), &std::collections::BTreeMap::new());
        assert_eq!(double_ruled, json!(1));
        assert_eq!(stale_teamed, json!(1));
        assert!(err.is_null());
        assert_eq!(
            ruled,
            vec!["DOUBLE RULED shared held by 2 live rows (lead-a, lead-b)"]
        );
        assert_eq!(
            stale,
            vec![
                "stale team shared on lead-dead (stored status orphaned); fno agents rm lead-dead"
            ]
        );

        let (double_ruled, stale_teamed, err, ruled, stale) = team_split_fields(
            Err("registry unreadable: boom".into()),
            &std::collections::BTreeMap::new(),
        );
        assert!(double_ruled.is_null());
        assert!(stale_teamed.is_null());
        assert_eq!(err, json!("registry unreadable: boom"));
        assert_eq!(
            ruled,
            vec!["team split read failed: registry unreadable: boom"]
        );
        assert!(stale.is_empty());

        let splits = crate::team_split::TeamSplits {
            double_ruled: vec![],
            stale: vec![crate::team_split::StaleCrown {
                row: "lead-fno-g6".into(),
                scope: "fno".into(),
                stored_status: "exited".into(),
            }],
        };
        let mut dead = std::collections::BTreeMap::new();
        dead.insert(
            "lead-fno-g6".to_string(),
            crate::team_split::DeadCallReading::Open {
                session_id: "278c9a89-11ed-49af-a6fb-371bb36e410d".to_string(),
                tool: "Bash".to_string(),
                at: "2026-09-21T08:21:13.913Z".to_string(),
                boot: Some("2026-09-21T13:33:58Z".to_string()),
            },
        );
        let (_, _, _, _, stale) = team_split_fields(Ok(splits), &dead);
        assert_eq!(
            stale,
            vec!["stale team fno on lead-fno-g6 (stored status exited): session 278c9a89-11ed-49af-a6fb-371bb36e410d stopped inside a Bash call made at 2026-09-21T08:21:13.913Z, before the last boot at 2026-09-21T13:33:58Z; fno agents resume lead-fno-g6 relaunches it, fno agents rm lead-fno-g6 drops the row and its team".to_string()]
        );

        let splits = crate::team_split::TeamSplits {
            double_ruled: vec![],
            stale: vec![crate::team_split::StaleCrown {
                row: "lead-gone".into(),
                scope: "fno".into(),
                stored_status: "exited".into(),
            }],
        };
        let mut dead = std::collections::BTreeMap::new();
        dead.insert(
            "lead-gone".to_string(),
            crate::team_split::DeadCallReading::Unread(
                "no transcript for session 278c9a89-11ed-49af-a6fb-371bb36e410d".to_string(),
            ),
        );
        let (_, _, _, _, stale) = team_split_fields(Ok(splits), &dead);
        assert_eq!(
            stale,
            vec!["stale team fno on lead-gone (stored status exited); fno agents rm lead-gone (tool-call reading: no transcript for session 278c9a89-11ed-49af-a6fb-371bb36e410d)".to_string()]
        );
    }

    // AC4-HP: the specimen line names the dead call and offers resume.

    // AC4-ERR: an unreadable reading appends the reason, never a clean read.

    /// The beat defaults under the equal-version trap this repo guards
    /// against: no `--scope`, no `--level`, no
    /// `--board-state`. The teamed caller's scope resolves from the
    /// registry (which may carry fields this binary predates), the level
    /// comes from the teamed row, and the board state defaults to the
    /// named scope's lead manifest when one exists.
    #[test]
    fn no_scope_resolves_scope_level_and_board_state_natively() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("fno-checkin-team-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("registry.json"),
            serde_json::json!({
                "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                "agents": [serde_json::json!({
                    "name": "lead-a792",
                    "cwd": "/tmp/x",
                    "created_at": "2026-09-01T00:00:00Z",
                    "status": "idle",
                    "harness": "claude",
                    "harness_session_id": "ses-team",
                    "crown_level": 2,
                    "crown_scope": "probe fleet",
                    "future_field": "x",
                })],
            })
            .to_string(),
        )
        .unwrap();
        let home_backup = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", &home);
        for (marker, _) in crate::claims::HARNESS_SESSION_MARKERS
            .iter()
            .chain(crate::claims::LEGACY_HARNESS_SESSION_MARKERS.iter())
        {
            std::env::remove_var(marker);
        }
        std::env::set_var("FNO_HARNESS_NAME", "claude");
        std::env::set_var("FNO_HARNESS_SESSION_ID", "ses-team");

        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let scope = crate::territory::canonical_scope("probe fleet");
        let expected_board = crate::paths::space_dir(&repo)
            .join("leads")
            .join(format!("{scope}.md"));
        std::fs::create_dir_all(expected_board.parent().unwrap()).unwrap();
        std::fs::write(&expected_board, "# lead\n").unwrap();

        let mut got_scope = String::new();
        let mut level: Option<i64> = None;
        let mut board_state: Option<PathBuf> = None;
        let resolved =
            resolve_missing_team_inputs(&mut got_scope, &mut level, &mut board_state, &repo);
        std::env::remove_var("FNO_HARNESS_NAME");
        std::env::remove_var("FNO_HARNESS_SESSION_ID");
        match home_backup {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        resolved.expect("the teamed caller's inputs must resolve");
        assert_eq!(got_scope, scope);
        assert_eq!(level, Some(2));
        assert_eq!(board_state, Some(expected_board));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit `--level` stands; a registry that cannot be read leaves
    /// the level unresolved with one stderr line naming the read error,
    /// never a failed beat.
    #[test]
    fn level_default_degrades_on_an_unreadable_registry() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("fno-checkin-level-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let home_backup = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", &home);

        let mut scope = String::from("probe");
        let mut level: Option<i64> = None;
        let mut board_state: Option<PathBuf> = None;
        let resolved = resolve_missing_team_inputs(&mut scope, &mut level, &mut board_state, &base);
        match home_backup {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
        resolved.expect("a levelless unreadable registry must not fail the beat");
        assert_eq!(level, None);
        assert_eq!(board_state, None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
