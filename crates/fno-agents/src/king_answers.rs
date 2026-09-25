//! The king check-in's scope-answer readings: which user decisions already
//! govern this crown's nodes, and why each quiet worker is quiet.
//!
//! The answered reading reads every answered question page in the questions
//! directory (state path `questions`), matching on NODE membership in the
//! crown scope, so a ruling recorded on another king's page still shows.
//! The quiet reading reads each quiet scope worker through
//! `fno agents peek`, printing its last RESULT line or help block; a
//! failed read keeps its row and says READER FAILED, never blanks.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Value};

/// How many peek records one worker's tail reads: deep enough that a RESULT
/// several tool-calls back still shows, small enough that a beat reading a
/// full court stays cheap.
const PEEK_RECORDS: usize = 40;

/// The rendered report cap, so one worker stays one line.
const REPORT_CAP_CHARS: usize = 200;

/// The crown scope's node ids, every status, straight from the graph. The
/// same compile the court fold runs, so a reading and the board cannot
/// disagree about what is in scope.
pub(crate) fn scope_node_ids(
    graph: &Path,
    cwd: &Path,
    scope: &str,
    level: Option<i64>,
) -> Result<BTreeSet<String>, String> {
    let level = level.ok_or("crown level unresolved")?;
    let entries = crate::backlog::api::rows(&crate::backlog::api::Store::new(&graph.to_path_buf()))
        .map_err(|e| format!("graph unreadable: {}", e.0))?;
    let projects = crate::king_board::project_map(cwd);
    crate::court_fold::compile_forced(scope, &entries, &projects, level)
}

/// Every readable question page in one directory, `(stem, text)`, conflict
/// markers excluded: the shape both question folds read.
fn read_question_pages(dir: &std::path::Path) -> Result<Vec<(String, String)>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("questions folder {} unreadable: {e}", dir.display()))?;
    let mut pages: Vec<(String, String)> = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if crate::attention_file::has_conflict_markers(&text) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        pages.push((stem.to_string(), text));
    }
    Ok(pages)
}

/// The answered reading: user decisions for scope nodes, from EVERY
/// answered page in the questions directory, any crown's page.
pub(crate) fn answered_reading(cwd: &Path, scope_ids: &BTreeSet<String>) -> Result<Value, String> {
    let dir = crate::state_path::resolve("questions", cwd)
        .ok_or("the questions state path did not resolve")?;
    let pages = read_question_pages(&dir)?;
    Ok(json!({"rows": answered_from_pages(&pages, scope_ids)}))
}

/// The pure fold: answered pages whose governing node is in scope, oldest
/// ruling first. The crown that asked is irrelevant here; the node carries
/// the scope.
fn answered_from_pages(pages: &[(String, String)], scope_ids: &BTreeSet<String>) -> Vec<Value> {
    let mut rows: Vec<(u64, Value)> = Vec::new();
    for (stem, text) in pages {
        let Some((front, _)) = crate::attention_file::parse_page(text) else {
            continue;
        };
        if front.status != "answered" {
            continue;
        }
        if !crate::attention_arm::stem_names_id(stem, &front.question_id) {
            continue;
        }
        // The governing node: blocks lead, the node field follows; a page
        // that names neither cannot be scope-matched.
        let node = front
            .blocks
            .first()
            .cloned()
            .filter(|b| !b.is_empty() && b != "none")
            .or_else(|| Some(front.node.clone()).filter(|n| !n.is_empty() && n != "none"));
        let Some(node) = node else {
            continue;
        };
        if !scope_ids.contains(&node) {
            continue;
        }
        let ts = front
            .answered_at
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| front.asked_at.clone());
        let epoch = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.timestamp().max(0) as u64)
            .unwrap_or(0);
        rows.push((
            epoch,
            json!({
                "node": node,
                "question_id": front.question_id,
                "question": front.title,
                "answer": one_line(front.answer.as_deref().unwrap_or(""), REPORT_CAP_CHARS),
                "ts": ts,
                "epoch": epoch,
            }),
        ));
    }
    rows.sort_by_key(|(epoch, _)| *epoch);
    rows.into_iter().map(|(_, v)| v).collect()
}

/// The quiet reading: why each quiet scope worker is quiet, one row per
/// worker, at most `MAX_COURT_ROWS` peeks per beat.
pub(crate) fn quiet_reading(
    top: Option<&Value>,
    scope_ids: &BTreeSet<String>,
) -> Result<Value, String> {
    let payload = top.ok_or("the workers reading failed; quiet workers are unreadable")?;
    let rows_all = payload_workers(payload)?;
    let quiet = quiet_scope_workers(rows_all, scope_ids);
    let total = quiet.len();
    let rows: Vec<Value> = quiet
        .into_iter()
        .take(crate::king_checkin::MAX_COURT_ROWS)
        .map(|(name, node)| {
            let line = peek_last_report(&name).unwrap_or_else(|e| format!("READER FAILED {e}"));
            json!({"worker": name, "node": node, "line": line})
        })
        .collect();
    Ok(json!({"quiet": total, "read": rows.len(), "rows": rows}))
}

/// The quiet rows in scope, `(worker name, node id)`, payload order. A
/// worker's node comes off the row when the join resolved, else off its
/// name; a row that names no scope node - kings, foreign workers - stays
/// out.
fn quiet_scope_workers(workers: &[Value], scope_ids: &BTreeSet<String>) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for w in workers {
        if crate::king_checkin::s_str(w, "status") != Some("quiet") {
            continue;
        }
        let Some(name) = crate::king_checkin::s_str(w, "name") else {
            continue;
        };
        let node = crate::king_checkin::s_str(w, "node")
            .map(str::to_string)
            .or_else(|| node_from_name(name));
        let Some(node) = node else {
            continue;
        };
        if !scope_ids.contains(&node) {
            continue;
        }
        rows.push((name.to_string(), node));
    }
    rows
}

/// The node id a spawned worker's name carries (`t-x-9999-glm`): the first
/// `x-` run of 4 to 8 hex chars bounded by non-alphanumerics.
fn node_from_name(name: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(pos) = name[from..].find("x-") {
        let abs = from + pos + 2;
        let end = name[abs..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map(|p| abs + p)
            .unwrap_or(name.len());
        let id = &name[abs..end];
        if (4..=8).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Some(format!("x-{id}"));
        }
        from = abs;
    }
    None
}

/// The worker's own last word, through the same door a human reads: one
/// `fno agents peek` tail. Nonzero exit is a failed read, named.
fn peek_last_report(handle: &str) -> Result<String, String> {
    let n = PEEK_RECORDS.to_string();
    let (code, out, err) =
        crate::king_checkin::fno_verb(&["agents", "peek", handle, "--json", "-n", &n])?;
    if code != 0 {
        return Err(stderr_fallback(&err, code));
    }
    last_report_from_peek(&out)
        .ok_or_else(|| format!("no RESULT or help block in the last {n} records"))
}

fn stderr_fallback(err: &str, code: i32) -> String {
    let cause = crate::king_checkin::stderr_cause(err);
    if cause == "no stderr" {
        format!("peek exited {code}")
    } else {
        cause
    }
}

/// The last report in a peek tail, newest record winning; inside one
/// record a RESULT line outranks a help block.
fn last_report_from_peek(out: &str) -> Option<String> {
    let mut report: Option<String> = None;
    for line in out.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(text) = rec.get("text").and_then(Value::as_str) else {
            continue;
        };
        if let Some(l) = last_result_line(text) {
            report = Some(l);
        } else if let Some(h) = help_line(text) {
            report = Some(h);
        }
    }
    report
}

/// The last `RESULT:` line in one record's text.
fn last_result_line(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            l.find("RESULT:")
                .map(|i| one_line(&l[i..], REPORT_CAP_CHARS))
        })
        .last()
}

/// One `<help ...>...</help>` block flattened to a line; an unclosed block
/// runs to the end of the text (the tail may have cut it).
fn help_line(text: &str) -> Option<String> {
    let start = text.find("<help")?;
    let end = text[start..]
        .find("</help>")
        .map(|i| start + i + "</help>".len())
        .unwrap_or(text.len());
    Some(one_line(&text[start..end], REPORT_CAP_CHARS))
}

/// Flatten to one display line: whitespace runs become single spaces,
/// then a char-safe cap.
fn one_line(text: &str, cap: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat
        .char_indices()
        .nth(cap)
        .map(|(i, _)| i)
        .unwrap_or(flat.len());
    let cut = &flat[..end];
    if cut.len() < flat.len() {
        format!("{cut}...")
    } else {
        cut.to_string()
    }
}

/// The held reading: this crown's open questions, read from the page
/// frontmatter the attention arm froze at write time. The folder comes from
/// items.json's `questions_dir`, so a king in any project reads the one
/// folder the daemon writes. A missing or stale items.json is a failed
/// reading, never `held: none`: the arm being down must not read as "no
/// questions".
pub(crate) fn held_reading(scope: &str) -> Result<Value, String> {
    let index_path = crate::attention_arm::attention_dir()
        .map_err(|e| e.to_string())?
        .join("items.json");
    let raw = std::fs::read_to_string(&index_path)
        .map_err(|_| "the attention arm has not written items.json".to_string())?;
    let index: Value =
        serde_json::from_str(&raw).map_err(|e| format!("items.json unreadable: {e}"))?;
    let as_of = index.get("as_of").and_then(Value::as_u64).unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(as_of);
    if age > 600 {
        return Err(format!(
            "items.json is {age}s old; the attention arm is not beating"
        ));
    }
    let dir = index
        .get("questions_dir")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .ok_or("items.json names no questions_dir")?;
    let mut pages = read_question_pages(std::path::Path::new(dir))?;
    pages.sort_by(|a, b| a.0.cmp(&b.0));
    let rows = held_from_pages(&pages, scope);
    Ok(json!({"open": rows.len(), "rows": rows}))
}

/// The pure fold: open pages whose question_id the stem names and whose
/// crown equals this check-in's canonical scope, oldest first. Rows carry
/// what the render needs to name the decide verb.
pub(crate) fn held_from_pages(pages: &[(String, String)], scope: &str) -> Vec<Value> {
    let canon = crate::territory::canonical_scope(scope);
    let mut rows: Vec<(u64, Value)> = Vec::new();
    for (stem, text) in pages {
        let Some((front, _)) = crate::attention_file::parse_page(text) else {
            continue;
        };
        if front.status != "open" {
            continue;
        }
        if !crate::attention_arm::stem_names_id(stem, &front.question_id) {
            continue;
        }
        if crate::territory::canonical_scope(&front.crown) != canon {
            continue;
        }
        let epoch = chrono::DateTime::parse_from_rfc3339(&front.asked_at)
            .map(|d| d.timestamp().max(0) as u64)
            .unwrap_or(0);
        let node = front
            .blocks
            .first()
            .cloned()
            .filter(|b| !b.is_empty() && b != "none")
            .or_else(|| Some(front.node.clone()).filter(|n| !n.is_empty() && n != "none"));
        rows.push((
            epoch,
            json!({
                "node": node,
                "question_id": front.question_id,
                "question": front.title,
                "ts": front.asked_at,
                "epoch": epoch,
            }),
        ));
    }
    rows.sort_by_key(|(epoch, _)| *epoch);
    rows.into_iter().map(|(_, v)| v).collect()
}

/// The workers payload, one `fno agents top --json` call shared by the
/// workers and quiet readings.
pub(crate) fn fetch_workers_payload() -> Result<Value, String> {
    let (_, out, err) = crate::king_checkin::fno_verb(&["agents", "top", "--json"])?;
    let payload: Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("top payload did not parse: {e}: {}", err.trim()))?;
    Ok(payload)
}

/// The payload's worker rows, behind the same positive-predicate guard the
/// summary runs: a top payload whose census failed carries no rows, and a
/// reading that answered from it would read as a quiet zero.
fn payload_workers(payload: &Value) -> Result<&[Value], String> {
    let predicate = payload
        .get("predicate")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim();
    let workers = payload.get("workers").and_then(|w| w.as_array());
    match (predicate.is_empty(), workers) {
        (false, Some(w)) => Ok(w),
        _ => Err("the top payload carries no positive predicate".into()),
    }
}

/// The summary the beat journals, folded from the shared top payload, so
/// the workers and quiet readings cost one `top` call between them.
pub(crate) fn workers_summary(payload: &Value) -> Result<Value, String> {
    let workers = payload_workers(payload)?;
    let mut oldest: Option<(f64, String)> = None;
    for w in workers {
        let age = w.get("status_age_s").and_then(|a| a.as_f64());
        let handle = w
            .get("handle")
            .and_then(|h| h.as_str())
            .or_else(|| crate::king_checkin::s_str(w, "name"))
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

/// The rows an `ok` rows-carrying reading holds, or its failure reason.
/// A reading the fixture lacks reads as absent-and-ok: the render's
/// `none` line, never a failure.
fn reading_rows(
    readings: &[crate::king_checkin::Reading],
    name: &str,
) -> (Option<String>, Value, Vec<Value>) {
    match readings.iter().find(|r| r.name == name) {
        Some(r) if !r.ok => (Some(r.error.clone()), Value::Null, Vec::new()),
        Some(r) => {
            let rows = r
                .value
                .get("rows")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            (None, r.value.clone(), rows)
        }
        None => (None, Value::Null, Vec::new()),
    }
}

/// The held render block: the decide and clear verbs one line per question.
pub(crate) fn held_lines(readings: &[crate::king_checkin::Reading]) -> Vec<String> {
    let (error, _value, rows) = reading_rows(readings, "held");
    let mut lines = Vec::new();
    if let Some(e) = error {
        lines.push(format!("READER FAILED held: {e}"));
        return lines;
    }
    // This crown's questions: the rows carry node, question id and ask
    // time; a row with a node names the decide verb, a row with none names
    // the clear verb.
    if rows.is_empty() {
        lines.push("held: none".into());
        return lines;
    }
    lines.push(format!("held: {} question(s) for this crown", rows.len()));
    for row in rows.iter().take(crate::king_checkin::MAX_COURT_ROWS) {
        let qid = crate::king_checkin::dash(row.get("question_id"));
        let node = row
            .get("node")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty() && *n != "none");
        match node {
            Some(n) => lines.push(format!(
                "  {n} on question {qid}; answer with: fno backlog decide {n} \"<ruling>\" --question-id {qid}"
            )),
            None => lines.push(format!(
                "  question {qid}; answer with: fno inbox outstanding clear {qid} --answer \"<answer>\" --authority crown"
            )),
        }
    }
    lines
}

/// The answered render block: one ruling per line, scoped by node.
pub(crate) fn answered_lines(readings: &[crate::king_checkin::Reading]) -> Vec<String> {
    let (error, _value, rows) = reading_rows(readings, "answered");
    let mut lines = Vec::new();
    if let Some(e) = error {
        lines.push(format!("READER FAILED answered: {e}"));
        return lines;
    }
    if rows.is_empty() {
        lines.push("answered: none in scope".into());
        return lines;
    }
    lines.push(format!(
        "answered: {} user decision(s) in scope",
        rows.len()
    ));
    for row in rows.iter().take(crate::king_checkin::MAX_COURT_ROWS) {
        lines.push(format!(
            "  {} ({}): {}",
            crate::king_checkin::dash(row.get("node")),
            crate::king_checkin::dash(row.get("question_id")),
            crate::king_checkin::dash(row.get("answer"))
        ));
    }
    let hidden = rows
        .len()
        .saturating_sub(crate::king_checkin::MAX_COURT_ROWS);
    if hidden > 0 {
        lines.push(format!("  ... {hidden} more rows cut"));
    }
    lines
}

/// The quiet render block: one line per quiet worker naming its own last
/// word, READER FAILED included.
pub(crate) fn quiet_lines(readings: &[crate::king_checkin::Reading]) -> Vec<String> {
    let (error, value, rows) = reading_rows(readings, "quiet_workers");
    let mut lines = Vec::new();
    if let Some(e) = error {
        lines.push(format!("READER FAILED quiet_workers: {e}"));
        return lines;
    }
    if rows.is_empty() {
        lines.push("quiet: none in scope".into());
        return lines;
    }
    let total = value
        .get("quiet")
        .and_then(Value::as_u64)
        .unwrap_or(rows.len() as u64);
    lines.push(format!("quiet: {total} worker(s) in scope"));
    for row in rows.iter().take(crate::king_checkin::MAX_COURT_ROWS) {
        lines.push(format!(
            "  {} ({}): {}",
            crate::king_checkin::dash(row.get("worker")),
            crate::king_checkin::dash(row.get("node")),
            crate::king_checkin::dash(row.get("line"))
        ));
    }
    let hidden = rows
        .len()
        .saturating_sub(crate::king_checkin::MAX_COURT_ROWS);
    if hidden > 0 {
        lines.push(format!("  ... {hidden} more rows cut"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(
        id: &str,
        crown: &str,
        status: &str,
        asked: &str,
        answered: Option<&str>,
        node: Option<&str>,
    ) -> (String, String) {
        let node_line = node.map(|n| format!("node: {n}\n")).unwrap_or_default();
        let answer_line = answered
            .map(|a| format!("answer: {a}\nanswered_at: {asked}\n"))
            .unwrap_or_default();
        let stem = format!("20260923-{id}-test-x-none");
        let text = format!(
            "---\nquestion_id: {id}\nkind: question\nstatus: {status}\ntitle: t-{id}\nasked_at: {asked}\n{answer_line}{node_line}crown: {crown}\n---\n\n# t-{id}\n"
        );
        (stem, text)
    }

    fn ids(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn answered_pages_match_on_node_from_any_crown() {
        let pages = vec![
            page(
                "q-foreign",
                "x-other-king",
                "answered",
                "2026-09-20T08:00:00Z",
                Some("keep the lane"),
                Some("x-1"),
            ),
            page(
                "q-mine",
                "x-this-king",
                "answered",
                "2026-09-21T08:00:00Z",
                Some("retract"),
                Some("x-2"),
            ),
            page(
                "q-open",
                "x-this-king",
                "open",
                "2026-09-22T08:00:00Z",
                None,
                Some("x-1"),
            ),
            page(
                "q-out",
                "x-this-king",
                "answered",
                "2026-09-19T08:00:00Z",
                Some("no"),
                Some("x-9"),
            ),
            page(
                "q-nodeless",
                "x-this-king",
                "answered",
                "2026-09-18T08:00:00Z",
                Some("maybe"),
                None,
            ),
        ];
        let rows = answered_from_pages(&pages, &ids(&["x-1", "x-2"]));
        assert_eq!(rows.len(), 2, "node membership, not page crown: {rows:?}");
        assert_eq!(
            rows[0].get("node"),
            Some(&json!("x-1")),
            "oldest ruling first"
        );
        assert_eq!(rows[1].get("question_id"), Some(&json!("q-mine")));
        assert_eq!(rows[0].get("answer"), Some(&json!("keep the lane")));
    }

    #[test]
    fn a_block_leads_when_the_node_field_is_absent() {
        let (stem, text) = page(
            "q-blocks",
            "x-k",
            "answered",
            "2026-09-20T08:00:00Z",
            Some("ship it"),
            None,
        );
        let body = text.replacen("crown:", "blocks:\n  - x-3\ncrown:", 1);
        let rows = answered_from_pages(&[(stem, body)], &ids(&["x-3"]));
        assert_eq!(rows.len(), 1, "blocks carry the node: {rows:?}");
        assert_eq!(rows[0].get("node"), Some(&json!("x-3")));
    }

    #[test]
    fn quiet_workers_read_nodes_off_names_and_drop_kings() {
        let payload = json!({"workers": [
            {"name": "t-x-9999-glm", "handle": "abc", "status": "quiet", "node": null},
            {"name": "t-x-2-w", "handle": "def", "status": "writing", "node": "x-2"},
            {"name": "king-4d9b-opus-g7", "handle": "ghi", "status": "quiet", "node": null},
            {"name": "review-2269", "handle": "jkl", "status": "quiet", "node": null},
            {"name": "x-3-worker", "handle": "mno", "status": "quiet", "node": "x-3"},
        ]});
        let rows = quiet_scope_workers(&payload, &ids(&["x-9999", "x-3"]));
        assert_eq!(
            rows,
            vec![
                ("t-x-9999-glm".to_string(), "x-9999".to_string()),
                ("x-3-worker".to_string(), "x-3".to_string()),
            ],
            "name join, live workers and kings out: {rows:?}"
        );
    }

    #[test]
    fn the_newest_record_carries_the_report() {
        let tail = concat!(
            "{\"role\": \"assistant\", \"text\": \"RESULT: BLOCKED need a ruling\"}\n",
            "{\"role\": \"assistant\", \"text\": \"[tool_use: Bash]\"}\n",
            "{\"role\": \"assistant\", \"text\": \"<help reason=\\\"scope\\\">which epic?</help>\"}\n",
        );
        assert_eq!(
            last_report_from_peek(tail).as_deref(),
            Some("<help reason=\"scope\">which epic?</help>"),
            "the later record wins"
        );
        let result_last = concat!(
            "{\"role\": \"assistant\", \"text\": \"<help reason=\\\"scope\\\">which epic?</help>\"}\n",
            "{\"role\": \"assistant\", \"text\": \"RESULT: BLOCKED need a ruling\\n\\nmore prose\"}\n",
        );
        assert_eq!(
            last_report_from_peek(result_last).as_deref(),
            Some("RESULT: BLOCKED need a ruling"),
            "RESULT is one line; the prose after it is cut"
        );
        assert_eq!(
            last_report_from_peek("{\"role\": \"assistant\", \"text\": \"thinking\"}"),
            None
        );
    }

    #[test]
    fn an_unterminated_help_block_still_reports() {
        let tail = "{\"role\": \"assistant\", \"text\": \"asking <help reason=\\\"budget\\\">need more room\"}\n";
        assert_eq!(
            last_report_from_peek(tail).as_deref(),
            Some("<help reason=\"budget\">need more room"),
        );
    }

    #[test]
    fn one_line_flattens_and_caps_at_a_char_boundary() {
        assert_eq!(one_line("a\n\n  b\tc", 200), "a b c");
        let long = "x".repeat(300);
        let cut = one_line(&long, 200);
        assert_eq!(cut.chars().count(), 203, "200 chars plus the ellipsis");
        assert!(cut.ends_with("..."));
    }

    #[test]
    fn quiet_reading_names_the_failure_instead_of_blanking() {
        let ids = ids(&["x-1"]);
        let err = quiet_reading(None, &ids).unwrap_err();
        assert!(err.contains("unreadable"), "err: {err}");
        let payload = json!({"workers": [
            {"name": "t-x-1-glm", "handle": "abc", "status": "quiet", "node": null},
        ]});
        // No peek in tests: the reading itself shells out only per row, so
        // assert the fold shape through quiet_scope_workers above; here the
        // dead-payload path stays named.
        let workers = payload.get("workers").and_then(Value::as_array).unwrap();
        let quiet = quiet_scope_workers(workers, &ids);
        assert_eq!(quiet.len(), 1);
    }

    #[test]
    fn a_payload_without_its_predicate_is_a_failed_read_not_a_quiet_zero() {
        let ids = ids(&["x-1"]);
        let err = quiet_reading(Some(&json!({"workers": null})), &ids).unwrap_err();
        assert!(err.contains("positive predicate"), "err: {err}");
        let err = quiet_reading(Some(&json!({"predicate": "", "workers": []})), &ids).unwrap_err();
        assert!(err.contains("positive predicate"), "err: {err}");
    }

    #[test]
    fn held_from_pages_lists_this_crowns_open_questions_oldest_first() {
        let pages = vec![
            held_page("q-new", "x-b", "open", "2026-09-23T12:00:00Z", Some("x-1")),
            held_page("q-old", "x-b", "open", "2026-09-22T08:00:00Z", None),
            held_page(
                "q-other",
                "fno",
                "open",
                "2026-09-21T08:00:00Z",
                Some("x-2"),
            ),
            held_page(
                "q-closed",
                "x-b",
                "answered",
                "2026-09-20T08:00:00Z",
                Some("x-3"),
            ),
        ];
        let rows = held_from_pages(&pages, "x-b");
        assert_eq!(rows.len(), 2, "AC11-HP: only x-b's open pages: {rows:?}");
        assert_eq!(
            rows[0].get("question_id").and_then(|v| v.as_str()),
            Some("q-old"),
            "oldest first"
        );
        assert_eq!(
            rows[1].get("question_id").and_then(|v| v.as_str()),
            Some("q-new")
        );
        // The node-less row reads as null so the render names the clear verb.
        assert_eq!(rows[0].get("node"), Some(&Value::Null));
    }

    fn held_page(
        id: &str,
        crown: &str,
        status: &str,
        asked_at: &str,
        node: Option<&str>,
    ) -> (String, String) {
        let node_line = match node {
            Some(n) => format!("node: {n}\n"),
            None => String::new(),
        };
        let stem = format!("20260923-{id}-test-x-none");
        let text = format!(
            "---\nquestion_id: {id}\nkind: question\nstatus: {status}\ntitle: t-{id}\nasked_at: {asked_at}\n{node_line}crown: {crown}\n---\n\n# t-{id}\n"
        );
        (stem, text)
    }
}
