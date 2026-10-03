//! Post-close side effects of the reconcile sweep: the retro sentinel, the
//! session_satisfied / human_touch events, and the tier-1 gate escapes.
//! Ported from graph/_reconcile.py (`write_retro_sentinel`,
//! `emit_session_satisfied_for_record`, `emit_human_touch_for_record`,
//! `emit_gate_escape_for_record`). Every leg is best-effort: a failure
//! prints one diagnostic and never aborts the close.

use md5::Digest;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::drift_scan::MergeDriftRecord;
use super::pr_link::repo_slug_from_url;
use md5::Md5;

/// Drop a per-node retro sentinel naming a node closed by reconcile.
///
/// The sentinel hands the judgment half (follow-up capture via
/// inbox/triage) to a later session's LLM/human pass. Reconcile must NOT
/// auto-create inbox lines or backlog nodes - that stays explicit.
/// Overwrites an existing sentinel for the same node (idempotent).
pub(crate) fn write_retro_sentinel(
    record: &MergeDriftRecord,
    sentinel_dir: &Path,
) -> std::io::Result<PathBuf> {
    debug_assert!(
        record.closeable(),
        "refusing to write sentinel for non-closeable {}",
        record.node_id
    );
    std::fs::create_dir_all(sentinel_dir)?;
    let path = sentinel_dir.join(format!("{}.json", record.node_id));
    let payload = json!({
        "node_id": record.node_id,
        "pr_number": record.pr_number,
        "pr_url": record.pr_url,
        "merged_at": record.merged_at,
        "plan_path": record.plan_path,
        "closed_by": "backlog-reconcile",
        "closed_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default() + "\n",
    )?;
    Ok(path)
}

/// The flat keys this module reads from a target-state.md frontmatter
/// (`status`, `pr_number`, `session_id`). Local parse so this module does
/// not couple to a private events helper. Empty on any parse problem.
fn read_state_frontmatter(state_path: &Path) -> Value {
    let Ok(text) = std::fs::read_to_string(state_path) else {
        return json!({});
    };
    let Some(rest) = text.strip_prefix("---") else {
        return json!({});
    };
    let rest = rest.trim_start_matches('\n');
    let Some(end) = rest.find("\n---") else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Quote-stripped scalar only: the state frontmatter is flat keys,
        // and nested rows name none of the three keys read here.
        let value = value.trim_matches('"');
        out.insert(key.trim().to_string(), json!(value));
    }
    Value::Object(out)
}

fn owning_state_path(record: &MergeDriftRecord) -> Option<PathBuf> {
    let cwd = record.cwd.as_deref().filter(|c| !c.is_empty())?;
    Some(Path::new(cwd).join(".fno").join("target-state.md"))
}

/// Emit a `session_satisfied{source:"pr_merge"}` event for the target
/// session that owned this record's worktree, when that session is still
/// live and its state file names this PR.
pub(crate) fn emit_session_satisfied_for_record(
    record: &MergeDriftRecord,
    reason: &str,
) -> Option<PathBuf> {
    let state_path = owning_state_path(record)?;
    if !state_path.exists() {
        return None;
    }
    let fields = read_state_frontmatter(&state_path);
    // Only satisfy a session that is still live. A COMPLETE/BLOCKED/ABORTED
    // session needs no nudge; emitting would be noise.
    if fields.get("status").and_then(Value::as_str).map(str::trim) != Some("IN_PROGRESS") {
        return None;
    }
    // Cross-check the resolved state file actually owns THIS node's PR
    // before nudging it: a cwd can be recycled, and without this guard we
    // would emit a pr_merge session_satisfied bound to another node's
    // session.
    let state_pr = fields
        .get("pr_number")
        .and_then(Value::as_str)
        .and_then(|s| s.trim().parse::<i64>().ok());
    if state_pr != Some(record.pr_number) {
        return None;
    }
    // The state file is the source of truth the stop hook compares
    // against, so read session_id from it (not record.session_id, which
    // may be stale).
    let session_id = fields
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "null");
    let Some(session_id) = session_id else {
        return None;
    };
    let Ok(state_bytes) = std::fs::read(&state_path) else {
        return None;
    };
    let gate_state_hash = format!("{:x}", Md5::digest(&state_bytes));

    let events_path = state_path.parent()?.join("events.jsonl");
    let emitter = crate::events::EventEmitter::new(events_path.clone(), "backlog");
    // The builder's data-level `source` is the TRIGGER ("pr_merge"); the
    // envelope-level producer rides the emitter's "backlog" scope.
    let mut data = json!({
        "source": "pr_merge",
        "reason": reason,
        "session_id": session_id,
        "gate_state_hash": gate_state_hash,
    });
    if let Some(url) = &record.pr_url {
        data["evidence_url"] = json!(url);
    }
    let result = emitter.emit("session_satisfied", &data);
    if let Err(e) = result {
        eprintln!(
            "reconcile: session_satisfied emit failed for {} (session={session_id}): {e}; merge close unaffected, defensive stop-hook probe is the backstop",
            record.node_id
        );
        return None;
    }
    Some(events_path)
}

/// Emit `human_touch{source:merge}` for a node reconcile just closed. An
/// out-of-band merge is a human steering action no loop performed.
/// Reconcile closes a node exactly once (a closed node is no longer
/// scanned), so this fires once per node.
pub(crate) fn emit_human_touch_for_record(record: &MergeDriftRecord) -> Option<PathBuf> {
    let events_path = match &record.cwd {
        Some(cwd) if !cwd.is_empty() => {
            let path = Path::new(cwd).join(".fno").join("events.jsonl");
            let emitter = crate::events::EventEmitter::new(path.clone(), "backlog");
            let result = emitter.emit(
                "human_touch",
                &json!({
                    "graph_node_id": record.node_id,
                    "source": "merge",
                    "resolution": "ok",
                }),
            );
            if let Err(e) = result {
                eprintln!(
                    "reconcile: human_touch emit failed for {}: {e}; merge close unaffected",
                    record.node_id
                );
                return None;
            }
            path
        }
        _ => {
            let cwd = std::env::current_dir().ok()?;
            let Some(space) = crate::paths::space_dir_opt(&cwd) else {
                return None;
            };
            let path = space.join("events.jsonl");
            let emitter = crate::events::EventEmitter::new(path.clone(), "backlog");
            if let Err(e) = emitter.emit(
                "human_touch",
                &json!({
                    "graph_node_id": record.node_id,
                    "source": "merge",
                    "resolution": "ok",
                }),
            ) {
                eprintln!(
                    "reconcile: human_touch emit failed for {}: {e}; merge close unaffected",
                    record.node_id
                );
                return None;
            }
            return None;
        }
    };
    Some(events_path)
}

/// The tier-1 gate-escape reasons, one spelling shared by the emit site and
/// its tests.
const GATE_ESCAPE_REASON_DEADBOT: &str = "dead-bot";
const GATE_ESCAPE_REASON_COVERAGE: &str = "zero-coverage";

/// True when `login` matches any configured reviewer: strip a trailing
/// `[bot]` suffix, then a case-insensitive substring check. The SAME
/// semantics as the ship gate, so a bot that reviewed under its gh
/// `[bot]` login is never flagged dead.
fn reviewer_matches(login: &str, reviewers: &[String]) -> bool {
    let mut stripped = login.to_lowercase();
    if let Some(base) = stripped.strip_suffix("[bot]") {
        stripped = base.to_string();
    }
    reviewers
        .iter()
        .any(|r| stripped.contains(r.to_lowercase().as_str()))
}

/// The lowercased set of logins that reviewed a PR (any review state
/// counts). A gh failure is a typed refusal so the caller fails OPEN: it
/// does not emit on uncertainty.
fn fetch_pr_review_logins(
    pr_number: i64,
    repo: Option<&str>,
    cwd: Option<&str>,
) -> Result<std::collections::BTreeSet<String>, super::merge_evidence::PrReadError> {
    use super::merge_evidence::PrReadError;
    use std::process::Command;
    let Some(gh) = super::drift_scan::gh_executable() else {
        return Err(PrReadError::new("gh CLI not found on PATH", "availability"));
    };
    let mut cmd = Command::new(gh);
    cmd.args(["pr", "view", &pr_number.to_string(), "--json", "reviews"]);
    if let Some(repo) = repo {
        cmd.args(["--repo", repo]);
    }
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        cmd.current_dir(cwd);
    }
    let out = cmd
        .output()
        .map_err(|e| PrReadError::new(format!("gh subprocess failed to launch: {e}"), ""))?;
    if !out.status.success() {
        return Err(PrReadError::new(
            format!(
                "gh pr view #{pr_number} reviews failed (rc={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            "",
        ));
    }
    let row: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| PrReadError::new(format!("gh stdout was not JSON: {e}"), "malformed"))?;
    let mut logins = std::collections::BTreeSet::new();
    for rev in row
        .get("reviews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(login) = rev
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
        {
            if !login.is_empty() {
                logins.insert(login.to_lowercase());
            }
        }
    }
    Ok(logins)
}

/// The latest `review_coverage` event data for a PR, or None (fail-open:
/// under-report rather than crash). Reads BOTH logs loop-check writes (the
/// project log unscoped; the global `~/.fno/events.jsonl` scoped by the
/// repo identity, since a bare PR number is only unique within one repo).
/// Newest ts wins; a ts tie takes the SAFER verdict (uncovered).
fn latest_review_coverage(pr_number: i64, events_path: &Path) -> Option<Value> {
    let mut best: Option<(String, Value)> = None;
    let push = |ts: &str, data: Value, best: &mut Option<(String, Value)>| {
        let newer = best
            .as_ref()
            .map(|(best_ts, best_data)| {
                ts > best_ts.as_str()
                    || (ts == best_ts.as_str() && !is_covered(&data) && is_covered(best_data))
            })
            .unwrap_or(true);
        if newer {
            *best = Some((ts.to_string(), data));
        }
    };
    let project_scan = scan_coverage_log(events_path, pr_number, None);
    if let Some((ts, data)) = project_scan {
        push(&ts, data, &mut best);
    }
    let global = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".fno").join("events.jsonl"));
    if let Some(global) = global {
        let cwd = events_path
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        let slug = cwd.and_then(|c| crate::finalize::repo_identity_from_git_remote(&c));
        if let Some((ts, data)) = scan_coverage_log(&global, pr_number, slug.as_deref()) {
            push(&ts, data, &mut best);
        }
    }
    best.map(|(_, data)| data)
}

fn is_covered(data: &Value) -> bool {
    data.get("coverage").and_then(Value::as_str) == Some("covered")
}

fn scan_coverage_log(
    path: &Path,
    pr_number: i64,
    repo_slug: Option<&str>,
) -> Option<(String, Value)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut best: Option<(String, Value)> = None;
    for line in text.lines() {
        if !line.contains("review_coverage") {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue; // one corrupt byte never wedges the scan
        };
        if ev.get("type").and_then(Value::as_str) != Some("review_coverage") {
            continue;
        }
        let data = ev.get("data").filter(|d| d.is_object()).cloned()?;
        if data.get("pr").and_then(Value::as_i64) != Some(pr_number) {
            continue;
        }
        if let Some(slug) = repo_slug {
            if data.get("repo").and_then(Value::as_str) != Some(slug) {
                continue;
            }
        }
        let ts = ev
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if best
            .as_ref()
            .map(|(best_ts, _)| ts >= *best_ts)
            .unwrap_or(true)
        {
            best = Some((ts, data));
        }
    }
    best
}

/// The zero-coverage escape: fire on a merged PR whose review_coverage
/// shows 0/unknown coverage, whether or not any bot was required. A
/// genuinely-reviewed PR does not escape; no event at all under-reports
/// (fail-open).
fn emit_zero_coverage_escape(record: &MergeDriftRecord, events_path: &Path) -> bool {
    let Some(cov) = latest_review_coverage(record.pr_number, events_path) else {
        return false;
    };
    if cov.get("coverage").and_then(Value::as_str) == Some("covered") {
        // The word decides, never the count: a budget spent on fail rounds
        // reads covered with passed_count 0.
        return false;
    }
    let count = cov
        .get("reviewed_count")
        .map(|c| match c {
            Value::Number(n) => n.to_string(),
            Value::Null => "unknown".to_string(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "0".to_string());
    let coverage = cov
        .get("coverage")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    gate_escape_verb(
        GATE_ESCAPE_REASON_COVERAGE,
        record.pr_number,
        Some(&record.node_id),
        &format!("merged with {coverage} review coverage ({count} reviewed) - nothing reviewed this diff"),
        events_path,
    );
    true
}

/// The shared emit: one `fno doctor event gate-escape` call. The verb, not
/// this shell, computes the dedup bucket and owns the canonical log +
/// durable failure-log, so a Rust-emitted and a Python-emitted escape in
/// the same bucket still collapse to one.
fn gate_escape_verb(
    reason: &str,
    pr: i64,
    node_id: Option<&str>,
    detail: &str,
    events_path: &Path,
) -> Option<PathBuf> {
    let mut cmd = std::process::Command::new(crate::scrape::fno_bin());
    cmd.args([
        "doctor",
        "event",
        "gate-escape",
        reason,
        "--pr-number",
        &pr.to_string(),
        "--detail",
        detail,
        "--events",
    ]);
    cmd.arg(events_path);
    if let Some(node_id) = node_id {
        cmd.args(["--node", node_id]);
    }
    let ok = cmd.stdout(std::process::Stdio::null()).status().ok()?;
    ok.success().then(|| events_path.to_path_buf())
}

/// Tier-1 auto-emit: a `gate_escape{reason:dead-bot}` when a required
/// review bot never reviewed a PR which merged out-of-band. The
/// zero-coverage escape fires first and short-circuits. A review-fetch
/// failure fails OPEN (under-report, never a guess), and the fetch/resolve
/// failure lands in the escape verb's durable failure log via the verb.
pub(crate) fn emit_gate_escape_for_record(
    record: &MergeDriftRecord,
    required_bots: &[String],
    events_path: Option<&Path>,
) -> Option<PathBuf> {
    if record.pr_number <= 0 {
        return None; // placeholder/unassigned PR number: nothing to escape
    }
    let events_path: PathBuf = match events_path {
        Some(p) => p.to_path_buf(),
        None => canonical_events_path(record.cwd.as_deref())?,
    };
    if emit_zero_coverage_escape(record, &events_path) {
        return Some(events_path);
    }
    let wanted: Vec<&String> = required_bots
        .iter()
        .filter(|b| !b.trim().is_empty())
        .collect();
    if wanted.is_empty() {
        return None; // nothing required, so nothing to escape (dead-bot)
    }
    let repo = repo_slug_from_url(record.pr_url.as_deref());
    let reviewed =
        match fetch_pr_review_logins(record.pr_number, repo.as_deref(), record.cwd.as_deref()) {
            Ok(r) => r,
            Err(_) => return None, // fail open: cannot tell whether a bot reviewed
        };
    let unmet: Vec<&String> = wanted
        .into_iter()
        .filter(|b| {
            !reviewed
                .iter()
                .any(|lg| reviewer_matches(lg, std::slice::from_ref(b)))
        })
        .collect();
    if unmet.is_empty() {
        return None; // every required bot reviewed; gate was met
    }
    let names: Vec<String> = {
        let mut v: Vec<String> = unmet.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };
    gate_escape_verb(
        GATE_ESCAPE_REASON_DEADBOT,
        record.pr_number,
        Some(&record.node_id),
        &format!("required bot(s) never reviewed: {}", names.join(", ")),
        &events_path,
    )
}

/// The events log a record's emit writes to: the record cwd's own
/// `.fno/events.jsonl` when it has one, else the space log.
fn canonical_events_path(cwd: Option<&str>) -> Option<PathBuf> {
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        return Some(Path::new(cwd).join(".fno").join("events.jsonl"));
    }
    let here = std::env::current_dir().ok()?;
    crate::paths::space_dir_opt(&here).map(|space| space.join("events.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(cwd: Option<&str>) -> MergeDriftRecord {
        MergeDriftRecord {
            node_id: "x-aaaa".into(),
            plan_path: None,
            pr_number: 42,
            pr_url: Some("https://github.com/o/r/pull/42".into()),
            pr_state: "MERGED".into(),
            merged_at: Some("2026-10-01T00:00:00Z".into()),
            error: None,
            session_id: None,
            cwd: cwd.map(str::to_string),
            merge_sha: None,
            changed_files: Vec::new(),
            files_truncated: false,
            error_kind: None,
            remedy: None,
        }
    }

    #[test]
    fn a_retro_sentinel_names_the_closer_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("fno-de-{}", std::process::id()));
        let first = write_retro_sentinel(&record(None), &dir).expect("written");
        assert_eq!(first.file_name().unwrap(), "x-aaaa.json");
        let text = std::fs::read_to_string(&first).expect("read");
        assert!(
            text.contains("\"closed_by\": \"backlog-reconcile\""),
            "{text}"
        );
        assert!(text.contains("\"pr_number\": 42"), "{text}");
        let second = write_retro_sentinel(&record(None), &dir).expect("overwritten");
        assert_eq!(first, second);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn state_file(dir: &Path, status: &str, pr: i64) -> PathBuf {
        let wt = dir.join("wt");
        std::fs::create_dir_all(wt.join(".fno")).expect("mkdir");
        let path = wt.join(".fno").join("target-state.md");
        std::fs::write(
            &path,
            format!(
                "---\nstatus: {status}\npr_number: {pr}\nsession_id: sess-abc123\n---\n\n# body\n"
            ),
        )
        .expect("write");
        path
    }

    /// The store is the acknowledgement boundary: read the envelopes back
    /// from `events.db` beside the journal, not the legacy file.
    fn store_text(journal: &std::path::Path) -> String {
        let store = crate::event_store::open_read(&crate::event_store::store_path(journal))
            .expect("store open");
        let mut stmt = store
            .prepare("SELECT line FROM events ORDER BY ts_ms")
            .expect("stmt");
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        rows.join("\n")
    }

    #[test]
    fn a_live_session_with_a_matching_pr_gets_the_satisfied_event() {
        let dir = std::env::temp_dir().join(format!("fno-de-live-{}", std::process::id()));
        let state = state_file(&dir, "IN_PROGRESS", 42);
        let wt = state.parent().unwrap().parent().unwrap();
        let events =
            emit_session_satisfied_for_record(&record(wt.to_str()), "reconcile_detected_merge");
        let journal = events.expect("emitted");
        let text = store_text(&journal);
        assert!(text.contains("session_satisfied"), "{text}");
        assert!(text.contains("sess-abc123"), "{text}");
        assert!(text.contains("gate_state_hash"), "{text}");
        assert!(
            text.contains("\"source\": \"pr_merge\"") || text.contains("\"source\":\"pr_merge\""),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_finished_session_or_a_foreign_pr_gets_no_event() {
        let dir = std::env::temp_dir().join(format!("fno-de-dead-{}", std::process::id()));
        let done = state_file(&dir, "COMPLETE", 42);
        let wt = done.parent().unwrap().parent().unwrap();
        assert!(emit_session_satisfied_for_record(&record(wt.to_str()), "r").is_none());
        // Same live session, but the merged PR is not the one the state
        // file names: no cross-session nudge.
        let live = state_file(&dir, "IN_PROGRESS", 7);
        let wt7 = live.parent().unwrap().parent().unwrap();
        assert!(emit_session_satisfied_for_record(&record(wt7.to_str()), "r").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_human_touch_event_lands_in_the_record_worktree() {
        let dir = std::env::temp_dir().join(format!("fno-de-touch-{}", std::process::id()));
        let wt = dir.join("wt");
        std::fs::create_dir_all(wt.join(".fno")).expect("mkdir");
        let events = emit_human_touch_for_record(&record(wt.to_str())).expect("emitted");
        let text = store_text(&events);
        assert!(text.contains("human_touch"), "{text}");
        assert!(text.contains("x-aaaa"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reviewer_matching_strips_the_bot_suffix_and_reads_substrings() {
        let bots = vec!["gemini".to_string()];
        assert!(reviewer_matches("gemini-code-assist[bot]", &bots));
        assert!(reviewer_matches("GEMINI[bot]", &bots));
        assert!(
            reviewer_matches("gemini-impersonator", &bots),
            "substring: matches"
        );
        assert!(!reviewer_matches("codex", &bots));
    }

    #[test]
    fn a_gate_escape_needs_a_pr_and_at_least_one_required_bot() {
        let mut no_pr = record(None);
        no_pr.pr_number = 0;
        assert!(emit_gate_escape_for_record(&no_pr, &["codex".into()], None).is_none());
        let rec = record(None);
        assert!(emit_gate_escape_for_record(&rec, &[], None).is_none());
    }
}
