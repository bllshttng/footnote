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

/// The answered reading: user decisions for scope nodes, from EVERY
/// answered page in the questions directory, any crown's page.
pub(crate) fn answered_reading(cwd: &Path, scope_ids: &BTreeSet<String>) -> Result<Value, String> {
    let dir = crate::state_path::resolve("questions", cwd)
        .ok_or("the questions state path did not resolve")?;
    let entries = std::fs::read_dir(&dir)
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
    let quiet = quiet_scope_workers(payload, scope_ids);
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
fn quiet_scope_workers(payload: &Value, scope_ids: &BTreeSet<String>) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for w in payload
        .get("workers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
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
        let quiet = quiet_scope_workers(&payload, &ids);
        assert_eq!(quiet.len(), 1);
    }
}
