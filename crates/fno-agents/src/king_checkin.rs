//! `king-checkin`: one verb runs the reign check-in body.
//!
//! Gathers the readings the reign skill names, prints them in a fixed order,
//! diffs against the previous canonical `reign_checkin` row, and emits that
//! row from the same values it printed. It reads, prints, diffs and
//! journals; it never decides (no spawn, no reap, no lever, no graph write).
//! Contract: docs/architecture/reign.md and skills/reign/SKILL.md.
//!
//! Python resolves the caller's crown scope and the paths Python owns
//! (journals, graph, handoffs, FAQs, the caller's king manifest) and relays
//! here, the same split `king-history` applies; the gather and the row write
//! are native so the Python-tree ratchet holds. The scope fold is the
//! `court-fold` fold in process, the previous row comes through the
//! `king-history` scan, and the board is the `board` payload read in
//! process, so the check-in cannot disagree with the surfaces a king already
//! reads.
//!
//! `king-checkin --scope SCOPE --events-path PATH [--events-path ...]
//!              --graph PATH --handoffs-dir PATH [--faqs-dir PATH]
//!              [--board-state PATH] [--emit-path PATH] [--no-emit] [--json]`
//!
//! rc 0 a completed beat (a failed row write warns, never fails the beat),
//! 2 usage failure.
use crate::court_fold::court_fold;
use crate::king_board::{read_board, BoardOpts};
use crate::king_history::REIGN_CHECKIN;
use crate::scrape::fno_bin;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The nine readings of the check-in body, in print order.
const READING_NAMES: [&str; 9] = [
    "user_notes",
    "board",
    "blocked_child",
    "court",
    "capacity",
    "workers",
    "crown",
    "drain",
    "main_ci",
];

/// The numeric keys this verb owns and diffs versus the previous beat.
const NUMERIC_DIFF_KEYS: [&str; 6] = [
    "open_prs",
    "free_claim_no_driver",
    "blocked",
    "active_nodes",
    "live_workers",
    "undelivered",
];

/// Render cap for the per-node rows a court line prints (the count in the
/// payload stays whole, only the rendered rows are cut, as the board does).
const MAX_COURT_ROWS: usize = 25;

const RED_CONCLUSIONS: [&str; 5] = [
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
];

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
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

struct Reading {
    name: &'static str,
    ok: bool,
    value: Value,
    error: String,
}

impl Reading {
    fn failed(name: &'static str, error: String) -> Self {
        Reading {
            name,
            ok: false,
            value: Value::Null,
            error,
        }
    }
    fn took(name: &'static str, value: Value) -> Self {
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
    /// The crown scope's level (epic or project), resolved Python-side from
    /// the registry row that holds the crown; the fold refuses a levelless
    /// crown, because compile_forced branches on it.
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

fn fno_verb(args: &[&str]) -> Result<(i32, String, String), String> {
    let mut argv = vec![fno_bin()];
    argv.extend(args.iter().map(std::ffi::OsString::from));
    run_capture(&argv)
}

/// The crown-keyed handoff doc for one scope, newest existing file first.
/// The key scheme matches the precompact writer (`config paths handoff
/// --scope`), so the two cannot drift; no doc yet is a failed reading, not a
/// placeholder beat.
fn crown_handoff_doc(ctx: &Ctx) -> Result<PathBuf, String> {
    let key = format!("crown-{}", sanitize_scope_key(&ctx.scope));
    if key == "crown-" {
        return Err("empty scope names no canon doc".into());
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let it = std::fs::read_dir(&ctx.handoffs_dir)
        .map_err(|_| format!("no canon handoff doc for scope {}", ctx.scope))?;
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
        .ok_or_else(|| format!("no canon handoff doc for scope {}", ctx.scope))
}

fn sanitize_scope_key(scope: &str) -> String {
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
    let doc = crown_handoff_doc(ctx)?;
    let text =
        std::fs::read_to_string(&doc).map_err(|e| format!("{}: unreadable: {e}", doc.display()))?;
    let block = extract_user_marker(&text)
        .ok_or_else(|| "user block marker missing or doc unreadable".to_string())?;
    if is_user_placeholder(&block) {
        return Ok(Value::Null);
    }
    Ok(Value::String(block))
}

fn board_queue<'a>(board: &'a Value, name: &str) -> Result<&'a Value, String> {
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
    let argv = vec![
        std::ffi::OsString::from("gh"),
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        "200".into(),
        "--json".into(),
        "number".into(),
    ];
    let (code, out, err) = run_capture(&argv).map_err(|e| e.to_string())?;
    if code != 0 {
        return Err(format!(
            "open PR listing failed: {}",
            err.trim().chars().take(120).collect::<String>()
        ));
    }
    let rows: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("open PR listing did not parse: {e}"))?;
    Ok(rows.as_array().map(|a| a.len() as i64).unwrap_or(0))
}

fn fetch_board(ctx: &Ctx) -> Result<Value, String> {
    let opts = BoardOpts {
        state_path: ctx.board_state.clone(),
        cwd: Some(ctx.cwd.clone()),
        ..Default::default()
    };
    Ok(read_board(&opts))
}

fn fetch_fold(ctx: &Ctx) -> Result<Value, String> {
    let crowns = vec![json!({"scope": ctx.scope, "level": ctx.level})];
    let payload = court_fold(&ctx.graph, &ctx.cwd, None, &crowns, "json")
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
    Ok(json!({"fold": mine, "stuck": payload.get("stuck").cloned().unwrap_or(Value::Null)}))
}

fn r_board(
    board: &Result<Value, String>,
    court: &Result<Value, String>,
    open_prs: Result<i64, String>,
) -> Result<Value, String> {
    let board = board.clone()?;
    let court = court.clone()?;
    let mut blocked_on: Vec<String> = Vec::new();
    let mut blocked = 0i64;
    if let Some(rows) = court
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

fn r_court(folded: &Result<Value, String>) -> Result<Value, String> {
    let court = folded.clone()?;
    let fold = &court["fold"];
    let total = fold.get("total").and_then(|t| t.as_i64()).unwrap_or(0);
    let done = fold
        .get("counts")
        .and_then(|c| c.get("done"))
        .and_then(|d| d.as_i64())
        .unwrap_or(0);
    let mut rows: Vec<Value> = Vec::new();
    for n in fold
        .get("nodes")
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
    {
        let status = s_str(n, "status").unwrap_or("");
        if status == "done" || status == "superseded" {
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
        "active_nodes": total - done,
        "total_nodes": total,
        "rows": rows,
    }))
}

fn r_capacity() -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["doctor", "footprint", "--json"])?;
    if out.trim().is_empty() {
        return Err(format!(
            "footprint unavailable: {}",
            err.trim().chars().take(120).collect::<String>()
        ));
    }
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("footprint payload did not parse: {e}"))?;
    let (_, gate_out, gate_err) = fno_verb(&["agents", "gate-status"])?;
    let gate: Value = serde_json::from_str(gate_out.trim())
        .map_err(|e| format!("gate payload did not parse: {e}: {}", gate_err.trim()))?;
    r_capacity_pair(&payload, &gate)
}

/// The pair from the two fetched payloads. The two instruments speak
/// different dialects of the same axis: footprint answers in the cpu-axis
/// vocabulary (admit/refuse), the gate probe in its whole-admission one
/// (accepted/refused). Disagreement compares meanings, never spellings.
fn r_capacity_pair(footprint_payload: &Value, gate_payload: &Value) -> Result<Value, String> {
    let footprint = footprint_payload
        .get("capacity_verdict")
        .cloned()
        .unwrap_or(Value::Null);
    let gate_verdict = s_str(gate_payload, "verdict").unwrap_or("").to_string();
    let fp_str = footprint.as_str().unwrap_or("").to_string();
    fn meaning(v: &str) -> &str {
        match v {
            "admit" | "accepted" => "admit",
            "refuse" | "refused" => "refuse",
            other => other,
        }
    }
    Ok(json!({
        "footprint": footprint,
        "gate": gate_verdict,
        "disagree": !gate_verdict.is_empty()
            && !fp_str.is_empty()
            && meaning(&fp_str) != meaning(&gate_verdict),
        "unparsed_lines": footprint_payload
            .get("unparsed_lines")
            .and_then(|u| u.as_i64())
            .unwrap_or(0),
    }))
}

fn r_workers() -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["agents", "top", "--json"])?;
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("top payload did not parse: {e}: {}", err.trim()))?;
    let predicate = payload
        .get("predicate")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let workers = payload.get("workers").and_then(|w| w.as_array());
    let workers = match (predicate.is_empty(), workers) {
        (false, Some(w)) => w,
        _ => return Err("the top payload carries no positive predicate".into()),
    };
    let mut oldest: Option<(f64, String)> = None;
    for w in workers {
        let age = w.get("status_age_s").and_then(|a| a.as_f64());
        let handle = w
            .get("handle")
            .and_then(|h| h.as_str())
            .or_else(|| s_str(w, "name"))
            .unwrap_or("");
        if let Some(age) = age {
            if oldest.as_ref().map(|(a, _)| age > *a).unwrap_or(true) {
                oldest = Some((age, handle.to_string()));
            }
        }
    }
    let (age, handle) = oldest.ok_or_else(|| "every status_age_s is null".to_string())?;
    Ok(json!({
        "live_workers": workers.len(),
        "oldest_worker_seen": format!("{}s {}", age as i64, handle),
    }))
}

fn r_crown() -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["agents", "court", "--json"])?;
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("court payload did not parse: {e}: {}", err.trim()))?;
    let summary = payload.get("summary").cloned().unwrap_or(json!({}));
    let mut anomalies: Vec<String> = Vec::new();
    for c in payload
        .get("crowns")
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
            anomalies.push(format!(
                "{holder} scope {scope} status {status} agree {agree:?}{reason}"
            ));
        }
    }
    Ok(json!({
        "total": summary.get("total").cloned().unwrap_or(Value::Null),
        "splits": summary.get("splits").cloned().unwrap_or(Value::Null),
        "disagreements": summary.get("disagreements").cloned().unwrap_or(Value::Null),
        "anomalies": anomalies,
    }))
}

fn r_drain(ctx: &Ctx) -> Result<Value, String> {
    let (_, out, err) = fno_verb(&["agents", "king", "drain", &ctx.scope])?;
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("drain payload did not parse: {e}: {}", err.trim()))?;
    Ok(payload.get("undelivered").cloned().unwrap_or(Value::Null))
}

fn owner_repo(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let tail = url.rsplit_once(':').map(|(_, t)| t).unwrap_or(url);
    let tail = tail.rsplit('/').take(2).collect::<Vec<_>>();
    if tail.len() == 2 && !tail[1].is_empty() {
        let repo = tail[0].trim_end_matches(".git");
        if !repo.is_empty() {
            return Ok(format!("{}/{repo}", tail[1]));
        }
    }
    Err(format!("cannot read owner/repo from origin url {url:?}"))
}

fn r_main_ci() -> Result<Value, String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let argv: Vec<std::ffi::OsString> = std::iter::once("git".into())
            .chain(args.iter().map(std::ffi::OsString::from))
            .collect();
        let (code, out, err) = run_capture(&argv).map_err(|e| e.to_string())?;
        if code != 0 {
            return Err(format!(
                "git {} failed: {}",
                args[0],
                err.trim().chars().take(120).collect::<String>()
            ));
        }
        Ok(out.trim().to_string())
    };
    let sha = git(&["rev-parse", "origin/main"])?;
    let owner_repo = owner_repo(&git(&["remote", "get-url", "origin"])?)?;
    let gh_json = |args: &[&str]| -> Result<Value, String> {
        let argv: Vec<std::ffi::OsString> = std::iter::once("gh".into())
            .chain(args.iter().map(std::ffi::OsString::from))
            .collect();
        let (code, out, err) = run_capture(&argv).map_err(|e| e.to_string())?;
        if code != 0 {
            return Err(format!(
                "gh api failed: {}",
                err.trim().chars().take(120).collect::<String>()
            ));
        }
        serde_json::from_str(out.trim()).map_err(|e| format!("gh api payload did not parse: {e}"))
    };
    let runs = gh_json(&[
        "api",
        &format!("repos/{owner_repo}/commits/{sha}/check-runs"),
    ])?;
    let status = gh_json(&["api", &format!("repos/{owner_repo}/commits/{sha}/status")])?;
    let check_runs = runs.get("check_runs").and_then(|c| c.as_array());
    let combined = s_str(&status, "state").unwrap_or("").to_lowercase();
    let red = check_runs
        .map(|runs| {
            runs.iter().any(|r| {
                let conclusion = s_str(r, "conclusion").unwrap_or("");
                RED_CONCLUSIONS.contains(&conclusion)
            })
        })
        .unwrap_or(false)
        || combined == "failure"
        || combined == "error";
    if red {
        return Ok(Value::String("red".into()));
    }
    let all_completed = check_runs
        .map(|runs| runs.iter().all(|r| s_str(r, "status") == Some("completed")))
        .unwrap_or(false);
    if check_runs.map(|r| !r.is_empty()).unwrap_or(false) && all_completed && combined == "success"
    {
        return Ok(Value::String("green".into()));
    }
    Ok(Value::String("pending".into()))
}

// ---------------------------------------------------------------------------
// gather

struct Beat {
    board: Result<Value, String>,
    folded: Result<Value, String>,
}

fn collect_readings(ctx: &Ctx) -> Vec<Reading> {
    let beat = Beat {
        board: fetch_board(ctx),
        folded: fetch_fold(ctx),
    };
    let mut readings: Vec<Reading> = Vec::new();
    let mut take = |name: &'static str, result: Result<Value, String>| {
        match result {
            Ok(value) => readings.push(Reading::took(name, value)),
            Err(error) => readings.push(Reading::failed(name, ReaderError(error).0)),
        };
    };
    take("user_notes", r_user_notes(ctx));
    take("board", r_board(&beat.board, &beat.folded, open_pr_count()));
    take("blocked_child", r_blocked_child(&beat.board));
    take("court", r_court(&beat.folded));
    take("capacity", r_capacity());
    take("workers", r_workers());
    take("crown", r_crown());
    take("drain", r_drain(ctx));
    take("main_ci", r_main_ci());
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
    if let Some(child) = get("blocked_child").filter(|r| r.ok) {
        data.insert("blocked_children".into(), child.value.clone());
    }
    if let Some(court) = get("court").filter(|r| r.ok) {
        data.insert("active_nodes".into(), court.value["active_nodes"].clone());
    }
    if let Some(workers) = get("workers").filter(|r| r.ok) {
        data.insert("live_workers".into(), workers.value["live_workers"].clone());
        data.insert(
            "oldest_worker_seen".into(),
            workers.value["oldest_worker_seen"].clone(),
        );
    }
    if let Some(capacity) = get("capacity").filter(|r| r.ok) {
        for key in ["footprint", "gate", "disagree", "unparsed_lines"] {
            let wire = match key {
                "footprint" => "capacity_footprint",
                "gate" => "capacity_gate",
                "disagree" => "capacity_disagree",
                _ => "unparsed_lines",
            };
            data.insert(
                wire.into(),
                capacity.value.get(key).cloned().unwrap_or(Value::Null),
            );
        }
    }
    if let Some(drain) = get("drain").filter(|r| r.ok) {
        data.insert("undelivered".into(), drain.value.clone());
    }
    if let Some(ci) = get("main_ci").filter(|r| r.ok) {
        data.insert("main_ci".into(), ci.value.clone());
    }
    let failed: Vec<&Reading> = readings.iter().filter(|r| !r.ok).collect();
    data.insert("coverage".into(), json!(readings.len() - failed.len()));
    data.insert(
        "readers_failed".into(),
        json!(failed.iter().map(|r| r.name).collect::<Vec<_>>()),
    );
    data
}

fn previous_row(ctx: &Ctx) -> (Option<Value>, String) {
    match crate::king_history::scan(&ctx.events_paths, &ctx.scope) {
        Ok(payload) => {
            let first = payload["events"]
                .as_array()
                .and_then(|e| e.first())
                .cloned();
            (first, String::new())
        }
        Err(e) => (None, e),
    }
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

// ---------------------------------------------------------------------------
// render

fn dash(v: Option<&Value>) -> String {
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

    match failed("court") {
        Some(r) => lines.push(format!("READER FAILED court: {}", r.error)),
        None => {
            let court = by_name("court").map(|r| &r.value).unwrap_or(&Value::Null);
            lines.push(format!(
                "{scope}: {} active of {} nodes",
                data.get("active_nodes")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".into()),
                dash(court.get("total_nodes")),
            ));
            let rows = court
                .get("rows")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            for row in rows.iter().take(MAX_COURT_ROWS) {
                lines.push(format!(
                    "  {} {} worker {} pr {} session {}",
                    dash(row.get("id")),
                    dash(row.get("status")),
                    dash(row.get("worker")),
                    dash(row.get("pr_number")),
                    dash(row.get("session")),
                ));
            }
            let hidden = rows.len().saturating_sub(MAX_COURT_ROWS);
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
                text.push_str(&format!(" (unparsed_lines {unparsed})"));
            }
            lines.push(text);
        }
    }

    match failed("workers") {
        Some(r) => {
            lines.push(format!("READER FAILED workers: {}", r.error));
            lines.push(format!("worker activity unmeasured: {}", r.error));
        }
        None => lines.push(format!(
            "workers: live {}, oldest activity {}",
            dash(data.get("live_workers")),
            dash(data.get("oldest_worker_seen")),
        )),
    }

    match failed("crown") {
        Some(r) => lines.push(format!("READER FAILED crown: {}", r.error)),
        None => {
            let crown = by_name("crown").map(|r| &r.value).unwrap_or(&Value::Null);
            lines.push(format!(
                "crown: {} crowns, splits {}, disagreements {}",
                dash(crown.get("total")),
                dash(crown.get("splits")),
                dash(crown.get("disagreements")),
            ));
            for anomaly in crown
                .get("anomalies")
                .and_then(|a| a.as_array())
                .into_iter()
                .flatten()
            {
                lines.push(format!(
                    "  {}",
                    s_str(anomaly, "scope")
                        .unwrap_or(&anomaly.to_string())
                        .to_string()
                ));
            }
        }
    }
    let _ = &by_name;

    match failed("drain") {
        Some(r) => lines.push(format!("READER FAILED drain: {}", r.error)),
        None => lines.push(format!(
            "drain: undelivered {}",
            dash(data.get("undelivered"))
        )),
    }
    match failed("main_ci") {
        Some(r) => lines.push(format!("READER FAILED main_ci: {}", r.error)),
        None => lines.push(format!("main ci: {}", dash(data.get("main_ci")))),
    }

    let coverage = data.get("coverage").and_then(|c| c.as_i64()).unwrap_or(0);
    lines.push(format!(
        "coverage: {coverage} of {} readings ok",
        READING_NAMES.len()
    ));
    let failed_names: Vec<&str> = data
        .get("readers_failed")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
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
        let parts: Vec<String> = NUMERIC_DIFF_KEYS
            .iter()
            .filter_map(|key| {
                let before = prev_data.get(*key)?;
                let after = data.get(*key)?;
                Some(format!("{key} {before} -> {after}"))
            })
            .collect();
        let ts = previous.as_ref().and_then(|p| s_str(p, "ts")).unwrap_or("");
        lines.push(format!("vs last beat ({ts}): {}", parts.join(", ")));
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
        if !name.starts_with("king-") || !name.ends_with(".md") {
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

const FAQ_PROMPT: &str = "fno agents king faq add --question \"...\" --answer \"...\" \
--specimen \"<node or PR>, <date>\" --exit \"<the change that retires this>\"";

fn emit_row(ctx: &Ctx, data: &Map<String, Value>) -> bool {
    let Some(path) = ctx.emit_path.as_ref() else {
        eprintln!("king-checkin: WARNING: no emit path, so the beat was not journalled");
        return false;
    };
    let forbidden = ["crown", "crown_scope", "result"]
        .iter()
        .any(|k| data.contains_key(*k));
    if forbidden {
        eprintln!("king-checkin: WARNING: reign_checkin row not emitted: a forbidden alias key is present");
        return false;
    }
    let row = json!({"ts": iso_now(), "type": REIGN_CHECKIN, "source": "loop", "data": data});
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            writeln!(f, "{row}")?;
            f.flush()
        });
    match written {
        Ok(()) => true,
        Err(e) => {
            // One write, one truth: a failed row is warned, never retried.
            // A retry that re-appends after a partial write would journal
            // the same beat twice, and the diff corpus cannot unsee that.
            eprintln!("king-checkin: WARNING: reign_checkin row not emitted: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// entry

/// `king-checkin --scope SCOPE --events-path PATH [--events-path ...]
///              --graph PATH --handoffs-dir PATH [--faqs-dir PATH]
///              [--board-state PATH] [--emit-path PATH] [--no-emit] [--json]`
///
/// rc 0 a completed beat, 2 usage failure.
pub fn run_king_checkin(args: &[String]) -> i32 {
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
    let mut i = 0;
    while i < args.len() {
        let flag = |name: &str| args[i] == name && i + 1 < args.len();
        if flag("--scope") {
            ctx.scope = args[i + 1].clone();
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
            eprintln!("fno-agents king-checkin: unknown flag {}", args[i]);
            eprintln!(
                "fno-agents king-checkin: --scope SCOPE --events-path PATH \
                 [--events-path ...] --graph PATH --handoffs-dir PATH \
                 [--faqs-dir PATH] [--board-state PATH] [--emit-path PATH] \
                 [--no-emit] [--json]"
            );
            return 2;
        }
    }
    if ctx.scope.is_empty()
        || ctx.events_paths.is_empty()
        || ctx.graph.as_os_str().is_empty()
        || ctx.handoffs_dir.as_os_str().is_empty()
    {
        eprintln!(
            "fno-agents king-checkin: --scope, --events-path, --graph and \
             --handoffs-dir are required"
        );
        return 2;
    }

    let ts = iso_now();
    let readings = collect_readings(&ctx);
    let data = build_data(&readings, &ctx.scope);
    let (previous, previous_error) = previous_row(&ctx);
    let previous_data = previous.as_ref().and_then(|p| p.get("data"));
    let change = derive_change(previous_data, &data, &previous_error);
    let mut data = data;
    data.insert("change".into(), json!(change));
    let mut lines = render_lines(
        &ctx.scope,
        &readings,
        &data,
        &previous,
        &previous_error,
        &change,
    );

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
        emit_row(&ctx, &data)
    } else {
        false
    };

    if as_json {
        let payload = json!({
            "scope": ctx.scope,
            "ts": ts,
            "coverage": data.get("coverage").cloned().unwrap_or(json!(0)),
            "readers_failed": readers_failed,
            "change": change,
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
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_key_sanitizes_like_the_writer() {
        assert_eq!(sanitize_scope_key("fno-x-6b7b epic"), "fno-x-6b7b-epic");
        assert_eq!(sanitize_scope_key("  --x--  "), "x");
        assert_eq!(sanitize_scope_key("///"), "");
    }

    #[test]
    fn user_marker_grabs_between_fences() {
        let doc = "intro\n<!-- fno:user -->\nline one\n<!-- /fno:user -->\ntail\n";
        assert_eq!(extract_user_marker(doc), Some("line one\n".to_string()));
    }

    #[test]
    fn unclosed_marker_runs_to_writer_heading() {
        let doc = "<!-- fno:user -->\nkept\n## Merge order and why (r1)\nnot kept\n";
        assert_eq!(extract_user_marker(doc), Some("kept\n".to_string()));
    }

    #[test]
    fn unclosed_marker_runs_to_next_fence_or_eof() {
        let doc = "<!-- fno:status -->\ns\n<!-- fno:user -->\nkept\n<!-- fno:other -->\n";
        assert_eq!(extract_user_marker(doc), Some("kept\n".to_string()));
        assert_eq!(
            extract_user_marker("<!-- fno:user -->\nkept to end"),
            Some("kept to end\n".to_string())
        );
        assert_eq!(extract_user_marker("no marker here"), None);
    }

    #[test]
    fn placeholder_only_block_reads_as_empty() {
        assert!(is_user_placeholder(
            "_(write here; the machine reads this every refresh and never edits it)_\n"
        ));
        assert!(!is_user_placeholder("a real note"));
        assert!(!is_user_placeholder(""));
    }

    #[test]
    fn owner_repo_reads_ssh_and_https_urls() {
        assert_eq!(
            owner_repo("git@github.com:own/repo.git").unwrap(),
            "own/repo"
        );
        assert_eq!(
            owner_repo("https://github.com/own/repo.git").unwrap(),
            "own/repo"
        );
        assert!(owner_repo("not a url").is_err());
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
            r#"{"fold": {"status": "ok", "total": 5, "counts": {"done": 2}, "nodes": [
            {"id": "x-2", "status": "in_progress", "worker": "w1", "pr_number": 7,
             "sessions": ["s1", "s2"]}
        ]}, "stuck": {"blocked": [{"id": "x-3", "blocked_by": ["x-1"]}]}}"#,
        )
        .unwrap()
    }

    #[test]
    fn board_and_court_readings_reduce_the_payloads() {
        let board = Ok(board_payload());
        let folded = Ok(fold_payload());
        let board_value = r_board(&board, &folded, Ok(7)).unwrap();
        assert_eq!(board_value["blocked"], json!(1));
        assert_eq!(board_value["blocked_on"], json!(["x-3 on x-1"]));
        assert_eq!(board_value["free_claim_no_driver"], json!(3));
        let court = r_court(&folded).unwrap();
        assert_eq!(court["active_nodes"], json!(3));
        assert_eq!(court["total_nodes"], json!(5));
        let rows = court["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["session"], json!("s1"));
    }

    fn sample_readings(board: Value, court: Value, cap: Value, workers: Value) -> Vec<Reading> {
        vec![
            Reading::took("user_notes", Value::Null),
            Reading::took("board", board),
            Reading::took("blocked_child", json!([{"node": "x-1"}])),
            Reading::took("court", court),
            Reading::took("capacity", cap),
            Reading::took("workers", workers),
            Reading::took(
                "crown",
                json!({"total": 2, "splits": 0, "disagreements": 0, "anomalies": []}),
            ),
            Reading::took("drain", json!(9)),
            Reading::took("main_ci", json!("green")),
        ]
    }

    #[test]
    fn disagree_compares_meanings_not_spellings() {
        let pair = |fp: &str, gate: &str| {
            r_capacity_pair(
                &json!({"capacity_verdict": fp, "unparsed_lines": 0}),
                &json!({"verdict": gate}),
            )
            .unwrap()
            .get("disagree")
            .and_then(|d| d.as_bool())
            .unwrap()
        };
        // The live healthy pair: footprint speaks cpu-axis, the gate speaks
        // whole-admission; same meaning, so no DISAGREE.
        assert!(!pair("admit", "accepted"));
        assert!(!pair("refuse", "refused"));
        assert!(pair("admit", "refused"));
        assert!(pair("refuse", "accepted"));
    }

    #[test]
    fn printed_numbers_and_row_come_from_one_dict() {
        let readings = sample_readings(
            json!({"open_prs": 7, "free_claim_no_driver": 1, "blocked": 2, "blocked_on": []}),
            json!({"active_nodes": 4, "total_nodes": 6, "rows": []}),
            json!({"footprint": "admit", "gate": "admit", "disagree": false, "unparsed_lines": 0}),
            json!({"live_workers": 3, "oldest_worker_seen": "90s w1"}),
        );
        let data = build_data(&readings, "x-a792");
        let lines = render_lines("x-a792", &readings, &data, &None, "", "no change");
        let board_line = lines.iter().find(|l| l.starts_with("board:")).unwrap();
        assert!(board_line.contains("open_prs 7"), "line: {board_line}");
        assert!(board_line.contains("blocked 2"));
        let workers_line = lines.iter().find(|l| l.starts_with("workers:")).unwrap();
        assert!(workers_line.contains("live 3"));
        assert_eq!(data.get("coverage"), Some(&json!(9)));
        assert_eq!(data.get("open_prs"), Some(&json!(7)));
    }

    #[test]
    fn failed_reader_prints_own_line_and_beat_continues() {
        let mut readings = sample_readings(
            Value::Null,
            json!({"active_nodes": 4, "total_nodes": 6, "rows": []}),
            json!({"footprint": "admit", "gate": "admit", "disagree": false, "unparsed_lines": 0}),
            json!({"live_workers": 3, "oldest_worker_seen": "90s w1"}),
        );
        readings[1] = Reading::failed("board", "board payload names no undriven_pr queue".into());
        let data = build_data(&readings, "x-a792");
        let change = derive_change(None, &data, "");
        let lines = render_lines("x-a792", &readings, &data, &None, "", &change);
        assert!(lines.iter().any(|l| l.starts_with("READER FAILED board:")));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("coverage: 8 of 9 readings ok")));
        assert!(lines.iter().any(|l| l.contains("failed readers: board")));
        assert_eq!(change, "no numeric movement; readings failed: board");
        assert_eq!(data.get("open_prs"), None);
        assert_eq!(data.get("readers_failed"), Some(&json!(["board"])));
    }

    #[test]
    fn no_change_refused_while_a_reading_failed() {
        let mut readings = sample_readings(
            json!({"open_prs": 7, "free_claim_no_driver": 1, "blocked": 2, "blocked_on": []}),
            json!({"active_nodes": 4, "total_nodes": 6, "rows": []}),
            json!({"footprint": "admit", "gate": "admit", "disagree": false, "unparsed_lines": 0}),
            json!({"live_workers": 3, "oldest_worker_seen": "90s w1"}),
        );
        readings[7] = Reading::failed("drain", "drain unreadable".into());
        let data = build_data(&readings, "x-a792");
        assert!(derive_change(None, &data, "").starts_with("no numeric movement; readings failed"));
    }

    fn journal(dir: &Path, rows: &[Value]) -> PathBuf {
        let path = dir.join("events.jsonl");
        let mut f = std::fs::File::create(path).unwrap();
        for r in rows {
            writeln!(f, "{r}").unwrap();
        }
        dir.join("events.jsonl")
    }

    fn prev_row() -> Value {
        json!({"ts": "2026-09-10T12:00:00Z", "type": "reign_checkin", "source": "loop",
            "data": {"scope": "x-a792", "change": "no change", "open_prs": 9,
                     "free_claim_no_driver": 1, "blocked": 2, "active_nodes": 4,
                     "live_workers": 3, "undelivered": 9}})
    }

    #[test]
    fn diff_against_previous_canonical_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal(dir.path(), &[prev_row()]);
        let ctx = Ctx {
            scope: "x-a792".into(),
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
        let (previous, err) = previous_row(&ctx);
        assert!(err.is_empty());
        let readings = sample_readings(
            json!({"open_prs": 7, "free_claim_no_driver": 1, "blocked": 2, "blocked_on": []}),
            json!({"active_nodes": 4, "total_nodes": 6, "rows": []}),
            json!({"footprint": "admit", "gate": "admit", "disagree": false, "unparsed_lines": 0}),
            json!({"live_workers": 3, "oldest_worker_seen": "90s w1"}),
        );
        let data = build_data(&readings, "x-a792");
        let change = derive_change(previous.as_ref().and_then(|p| p.get("data")), &data, "");
        assert_eq!(change, "moved: open_prs 9 -> 7");
        let lines = render_lines("x-a792", &readings, &data, &previous, "", &change);
        let diff_line = lines
            .iter()
            .find(|l| l.starts_with("vs last beat (2026-09-10T12:00:00Z)"))
            .unwrap();
        assert!(diff_line.contains("open_prs 9 -> 7"), "line: {diff_line}");
    }

    #[test]
    fn faq_scope_line_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("king-s-1234.md");
        std::fs::write(&path, "---\ncreated: t\nscope: x-a792\n---\n\n# q\n").unwrap();
        std::fs::write(dir.path().join("king-other.md"), "---\nscope: other\n---\n").unwrap();
        let entries = faq_entries_for_scope(dir.path(), "x-a792");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].starts_with("---"));
    }
}
