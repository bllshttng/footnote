//! Which of a node's prose fields is the current answer about that node.
//! `details` is the original filing and nothing rewrites it; the newest
//! thread row is the live reading (the feed is the state), with
//! current_state as the legacy fallback; `plan_path` names the document
//! that owns the file list once a plan exists. A reader that sees only the
//! first of those reaches a confident wrong answer.

use serde_json::Value;

pub const READING_KEY: &str = "_reading";

/// The newest thread row carrying prose (any kind), by list position.
fn newest_thread_row(row: &Value) -> Option<&Value> {
    row.get("progress_notes")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .filter(|r| {
                    r.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| !t.is_empty())
                })
                .last()
        })
}

/// The one-line precedence marker, or `None` when the row has neither a
/// thread, a current state, nor a plan (an ordinary row's bytes are then
/// unchanged).
pub fn reading_for(row: &Value) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(newest) = newest_thread_row(row) {
        clauses.push(format!(
            "the thread's newest row ({}, {}) is the live reading",
            newest.get("kind").and_then(Value::as_str).unwrap_or("note"),
            newest
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        ));
    } else if let Some(view) = crate::backlog::node_state::read_state(row) {
        if !view.body.is_empty() {
            clauses.push(format!(
                "current_state (rev {}, {}) is the live reading",
                view.revision,
                view.updated_at.as_deref().unwrap_or("unknown"),
            ));
        }
    }
    if row
        .get("plan_path")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty())
    {
        clauses.push("plan_path is authoritative for the file list".to_string());
    }
    if clauses.is_empty() {
        return None;
    }
    if row
        .get("details")
        .and_then(Value::as_str)
        .is_some_and(|d| !d.is_empty())
    {
        clauses.push("details is the original filing and may be stale".to_string());
    }
    Some(clauses.join("; "))
}

/// Insert `READING_KEY` as the FIRST key of every row that has a reading.
/// Read-only callers only: the marker is derived and must never be persisted.
pub fn attach_reading(rows: &mut [Value]) {
    for row in rows.iter_mut() {
        if let (Some(reading), Some(obj)) = (reading_for(row), row.as_object_mut()) {
            obj.shift_insert(0, READING_KEY.to_string(), Value::String(reading));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_row() -> Value {
        json!({
            "id": "x-aaaa",
            "details": "the fix path is cli/src/fno/pr/_sync_canonical.py",
            "plan_path": "plans/20260917-reading.md",
            "current_state": {
                "body": "work moved to the rust merge-close path",
                "revision": 2,
                "updated_at": "2026-09-17T22:41",
            },
        })
    }

    #[test]
    fn a_full_row_reads_state_then_plan_then_stale_details() {
        let reading = reading_for(&full_row()).expect("reading");
        assert_eq!(
            reading,
            "current_state (rev 2, 2026-09-17T22:41) is the live reading; \
             plan_path is authoritative for the file list; \
             details is the original filing and may be stale"
        );
    }

    #[test]
    fn a_stateless_planless_row_has_no_reading() {
        let row = json!({"id": "x-aaaa", "details": "just a filing"});
        assert_eq!(reading_for(&row), None);
    }

    #[test]
    fn an_empty_details_row_suppresses_the_stale_clause() {
        let row = json!({
            "id": "x-aaaa",
            "plan_path": "plans/one.md",
            "details": "",
        });
        assert_eq!(
            reading_for(&row).as_deref(),
            Some("plan_path is authoritative for the file list")
        );
    }

    #[test]
    fn a_state_with_an_empty_body_does_not_count() {
        let row = json!({
            "id": "x-aaaa",
            "current_state": {"body": "", "revision": 1, "updated_at": "t"},
        });
        assert_eq!(reading_for(&row), None);
        // the thread outranks a stale state - the newest row is the
        // live reading even when current_state is present and empty.
        let threaded = json!({
            "id": "x-aaaa",
            "current_state": {"body": "stale", "revision": 1, "updated_at": "t"},
            "progress_notes": [
                {"ts": "2026-10-02T10:00:00Z", "text": "routed the FAIL", "kind": "progress"},
            ],
        });
        let reading = reading_for(&threaded).expect("threaded row has a reading");
        assert!(
            reading.starts_with("the thread's newest row (progress, 2026-10-02T10:00:00Z)"),
            "the newest row is the live reading: {reading}"
        );
    }

    #[test]
    fn attach_puts_the_marker_first_and_touches_nothing_when_absent() {
        let mut rows = vec![full_row(), json!({"id": "x-bbbb", "details": "plain"})];
        let plain_before = rows[1].clone();
        attach_reading(&mut rows);
        assert_eq!(
            rows[0]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .map(String::as_str),
            Some(READING_KEY)
        );
        assert!(rows[0][READING_KEY]
            .as_str()
            .unwrap()
            .ends_with("details is the original filing and may be stale"));
        assert_eq!(
            rows[1], plain_before,
            "a row with no reading is byte-for-byte unchanged"
        );
    }
}
