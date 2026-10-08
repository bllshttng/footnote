//! The `agent.transcript-append` op and the stored-transcript readers. Split
//! out of daemon.rs (shrink-only) so transcript-side additions land here,
//! never back in the daemon file.
//!
//! The lane: a sandboxed session's turn hook pushes raw transcript lines over
//! the daemon socket; the daemon wraps each into a judged `transcript_record`
//! envelope and commits it through the event store. Readers
//! (`capped_tail`/`codex_capped_tail`, the activity fold) read stored records
//! first and fall back to the file. A session with no stored rows and no
//! readable file answers `unknown`, never `idle`: an idle verdict for a live
//! sandboxed worker would park or reap it while it works.

use super::*;

/// The store row's envelope type (schema: `transcript_record`).
pub(crate) const TRANSCRIPT_RECORD_TYPE: &str = "transcript_record";

/// The record line's own timestamp: claude rows carry `timestamp`, codex
/// rows carry `timestamp`, so a retried push re-mints the identical
/// envelope. `None` sends the caller's fallback.
fn line_timestamp(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(String::from)
}

/// `agent.transcript-append` - store raw transcript lines for one session.
/// Each record rides the event store's idempotent append (the event id is the
/// sha256 of the envelope line, so a hook retry is an idempotent hit, never a
/// duplicate). The judge (`validate_envelope`) owns the schema; a refused
/// record answers `InvalidParams` naming the diagnostic.
pub(crate) fn handle_transcript_append(ctx: &Ctx, req: &Request) -> Response {
    let Some(session_id) = req
        .params
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
    else {
        return Response::err(req.id, ErrorCode::InvalidParams, "missing `session_id`");
    };
    let Some(records) = req.params.get("records").and_then(|v| v.as_array()) else {
        return Response::err(req.id, ErrorCode::InvalidParams, "missing `records`");
    };
    let journal = ctx.home.transcripts_journal();
    let mut stored = 0u64;
    let mut deduped = 0u64;
    let mut refused: Vec<String> = Vec::new();
    for record in records {
        let Some(line) = record.get("line").and_then(|v| v.as_str()) else {
            refused.push("a record carries no `line`".to_string());
            continue;
        };
        // The envelope's content hash IS the idempotency key, so the ts must
        // be a pure function of the record: retrying a lost reply re-sends
        // the same line and must re-mint the same envelope. The record's own
        // timestamp (the harness wrote it inside the line) wins; `ts` on the
        // record next; only a record with neither mints now(), and a retry
        // of such a row is accepted as the rare duplicate.
        let ts = match record.get("ts").and_then(|v| v.as_str()) {
            Some(ts) => ts.to_string(),
            None => line_timestamp(line).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        };
        let envelope = json!({
            "ts": ts,
            "type": TRANSCRIPT_RECORD_TYPE,
            "source": "hook",
            "data": {
                "session_id": session_id,
                "line": line,
            },
        });
        match crate::event_store::append_envelope(&journal, &envelope.to_string(), None) {
            Ok(receipt) => {
                if receipt.inserted {
                    stored += 1;
                } else {
                    deduped += 1;
                }
            }
            Err(msg) => refused.push(msg),
        }
    }
    Response::ok(
        req.id,
        json!({
            "session_id": session_id,
            "stored": stored,
            "deduped": deduped,
            "refused": refused,
        }),
    )
}

/// The stored transcript lines for one session, oldest first, bounded to the
/// same tail window the file readers read (a scan needs the newest decisive
/// rows, never the whole history). `None` when the store has no row for the
/// session (the file fallback then owns the read) or the store cannot be
/// read. Judge-refused rows are excluded: a refused row is evidence of a
/// writer bug, never a transcript fact.
pub(crate) fn stored_lines(journal: &Path, session_id: &str) -> Option<Vec<String>> {
    let query = crate::event_store::EventQuery {
        types: vec![TRANSCRIPT_RECORD_TYPE.to_string()],
        session_id: Some(session_id.to_string()),
        include_rejected: false,
        ..Default::default()
    };
    let rows = crate::event_store::query_events(journal, &query).ok()?;
    if rows.is_empty() {
        return None;
    }
    // Tail window in bytes: keep appending and shed from the front once the
    // kept lines outgrow the window, so memory tracks the file readers'
    // [`TAIL_BYTES`] bound, not the session's age.
    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut kept_bytes = 0u64;
    for row in rows {
        let value: Value = serde_json::from_str(&row.line).ok()?;
        let line = value.get("data")?.get("line")?.as_str()?.to_string();
        kept_bytes += line.len() as u64;
        kept.push_back(line);
        while kept_bytes > TAIL_WINDOW_BYTES {
            if let Some(front) = kept.pop_front() {
                kept_bytes -= front.len() as u64;
            }
        }
    }
    Some(kept.into())
}

/// The stored-tail window, matched to the file readers' tail so a stored
/// verdict covers the same rows a file read would.
const TAIL_WINDOW_BYTES: u64 = 1024 * 1024;

/// The newest stored record's wall-clock ms for one session. `None` when the
/// store holds nothing for the session or is unreadable.
pub(crate) fn newest_stored_ts_ms(journal: &Path, session_id: &str) -> Option<i64> {
    let query = crate::event_store::EventQuery {
        types: vec![TRANSCRIPT_RECORD_TYPE.to_string()],
        session_id: Some(session_id).map(String::from),
        include_rejected: false,
        ..Default::default()
    };
    let rows = crate::event_store::query_events(journal, &query).ok()?;
    rows.last().map(|row| row.ts_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("transcript-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_with_journal() -> (Ctx, PathBuf) {
        let home = crate::paths::AgentsHome::at(tempdir("home"));
        let journal = home.transcripts_journal();
        (
            super::super::tests::test_ctx(home, PathBuf::from("fno-agents-worker")),
            journal,
        )
    }

    fn append_req(session_id: &str, lines: &[&str]) -> Request {
        let records: Vec<Value> = lines.iter().map(|line| json!({ "line": line })).collect();
        Request::new(
            1,
            "agent.transcript-append",
            json!({ "session_id": session_id, "records": records }),
        )
    }

    #[test]
    fn append_stores_judged_records_and_answers_counts() {
        let (ctx, journal) = ctx_with_journal();
        let req = append_req(
            "sess-a",
            &[
                r#"{"type":"assistant","timestamp":"2026-10-08T01:00:00.000Z"}"#,
                r#"{"type":"user","timestamp":"2026-10-08T01:00:01.000Z"}"#,
            ],
        );
        let res = handle_transcript_append(&ctx, &req);
        let body = res.result().expect("ok");
        assert_eq!(body["stored"], 2);
        assert_eq!(body["deduped"], 0);
        assert_eq!(body["refused"].as_array().map(Vec::len), Some(0));
        // The same push again is idempotent hits, never duplicates.
        let res = handle_transcript_append(&ctx, &req);
        let body = res.result().expect("ok");
        assert_eq!(body["deduped"], 2);
        // A session with no rows reads None: the file fallback owns the read.
        assert_eq!(stored_lines(&journal, "sess-b"), None);
        let lines = stored_lines(&journal, "sess-a").expect("stored");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"type\":\"assistant\""));
    }

    #[test]
    fn append_refuses_an_unjudgeable_record_and_names_it() {
        let (ctx, _journal) = ctx_with_journal();
        let req = Request::new(
            1,
            "agent.transcript-append",
            json!({ "session_id": "sess-a", "records": [{ "noline": true }] }),
        );
        let res = handle_transcript_append(&ctx, &req);
        let body = res.result().expect("ok");
        assert_eq!(body["stored"], 0);
        assert_eq!(body["refused"].as_array().map(|r| r.len()), Some(1));
    }

    #[test]
    fn newest_ts_answers_the_last_committed_row() {
        let (ctx, journal) = ctx_with_journal();
        assert_eq!(newest_stored_ts_ms(&journal, "sess-c"), None);
        let req = append_req("sess-c", &[r#"{"type":"user"}"#]);
        handle_transcript_append(&ctx, &req);
        assert!(newest_stored_ts_ms(&journal, "sess-c").is_some());
    }
}
