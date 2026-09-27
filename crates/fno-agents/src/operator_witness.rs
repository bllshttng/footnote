//! Did a person type this turn? The `operator_submit` witness join: the mux
//! appends one `operator_submit` row per human Enter at its input choke
//! point, and the fold binds transcript turns to those rows by session id
//! and submit_ms. A turn with no submit and no injected shape reads
//! `unknown`, never `operator`: the label is witnessed or it does not exist.

use std::collections::HashMap;
use std::path::Path;

/// How far before a turn a submit may sit and still bind it, and how far
/// after. The negative side absorbs clock rounding; the positive side covers
/// per-turn hook latency: a submit the harness queues while busy and writes
/// later than 30s reads unknown - the safe direction. Measured 2026-09-21
/// over 52 `agent_raw_inject` rows: 50 landed within 0.5s of the transport
/// write, and that event is written AFTER the transport confirms, so the
/// negative side ran to -4.83s; operator_submit is written when Enter
/// reaches the server, BEFORE the harness writes the turn row, so the
/// submit side of the window may be tighter, and the receipt's
/// `latency_p95_ms` is the evidence to retune it from.
pub(crate) const SUBMIT_WINDOW_BEFORE_MS: i64 = 2_000;
pub(crate) const SUBMIT_WINDOW_AFTER_MS: i64 = 30_000;

/// The witness receipt on the fold's Report: what the mux saw, what the fold
/// bound, and what stayed outside the witness.
#[derive(Debug, serde::Serialize)]
pub(crate) struct WitnessReceipt {
    /// operator_submit rows inside the --days window.
    pub(crate) submits: usize,
    /// Submits bound to a turn (a witnessed operator turn, or a submit a
    /// command turn consumed so it cannot bind a later machine turn).
    pub(crate) bound: usize,
    /// In-window submits no turn claimed (a menu Enter, or a turn the
    /// harness never wrote).
    pub(crate) unbound: usize,
    /// `resolution: unresolved` rows: shell-pane submits nothing can join.
    pub(crate) unresolved: usize,
    /// Sessions with unknown turns and no submit row at all: sessions typed
    /// outside the mux.
    pub(crate) unwitnessed_sessions: usize,
    /// p95 of turn_ms - submit_ms over bound turns; the retune evidence.
    pub(crate) latency_p95_ms: Option<u64>,
}

/// The `operator_submit` rows the fold joins against: keyed by transcript
/// session id to sorted submit times, one bind per row.
pub(crate) struct SubmitIndex {
    /// transcript session id -> sorted submit_ms (only `resolution: ok` rows
    /// with a session id; nothing can join an unresolved shell row).
    by_session: HashMap<String, Vec<i64>>,
    /// per session, the next unconsumed index into `by_session`. One submit
    /// binds at most one turn, so consumed rows never bind again.
    cursors: HashMap<String, usize>,
    /// Every row's submit_ms (any resolution): the window count `submits`.
    all_submit_ms: Vec<i64>,
    /// Rows with `resolution: unresolved`: submits the mux saw but no row
    /// bound - the from-the-mux measure of shell-pane use.
    unresolved: usize,
    /// Turns bound so far, and their latencies (turn_ms - submit_ms).
    bound: usize,
    latencies_ms: Vec<i64>,
}

impl SubmitIndex {
    /// The index over `journal` (the agents events.jsonl, complete across
    /// rotation). Unreadable means empty: the fold degrades to all-unknown,
    /// never a failed fold.
    pub(crate) fn load(journal: &Path) -> SubmitIndex {
        let text = crate::event_store::journal_text(journal, &["operator_submit"]);
        let mut index = SubmitIndex::empty();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("operator_submit") {
                continue;
            }
            let Some(submit_ms) = v.pointer("/data/submit_ms").and_then(|m| m.as_i64()) else {
                continue;
            };
            index.all_submit_ms.push(submit_ms);
            match v.pointer("/data/resolution").and_then(|r| r.as_str()) {
                Some("unresolved") => index.unresolved += 1,
                Some("ok") => {
                    if let Some(session) =
                        v.pointer("/data/harness_session").and_then(|s| s.as_str())
                    {
                        index
                            .by_session
                            .entry(session.to_string())
                            .or_default()
                            .push(submit_ms);
                    }
                }
                _ => {}
            }
        }
        for subs in index.by_session.values_mut() {
            subs.sort_unstable();
        }
        index
    }

    /// An empty index: every turn reads unknown, the loop still runs.
    pub(crate) fn empty() -> SubmitIndex {
        SubmitIndex {
            by_session: HashMap::new(),
            cursors: HashMap::new(),
            all_submit_ms: Vec::new(),
            unresolved: 0,
            bound: 0,
            latencies_ms: Vec::new(),
        }
    }

    /// Whether any witnessed submit names this transcript session at all.
    pub(crate) fn has_session(&self, session: &str) -> bool {
        self.by_session.contains_key(session)
    }

    /// Bind one turn to the earliest unconsumed submit with `turn_ms -
    /// submit_ms` in [-2000, +30000] ms, and consume it: one submit binds at
    /// most one turn. `None` reads unknown; `Some` carries the latency.
    /// Turns are handed in transcript order.
    pub(crate) fn bind(&mut self, session: &str, turn_ms: i64) -> Option<i64> {
        let subs = self.by_session.get_mut(session)?;
        let cursor = self.cursors.entry(session.to_string()).or_insert(0);
        // Submits dead for every future turn (older than turn-30s) leave the
        // window for good: skip them so they can never bind a later turn.
        while *cursor < subs.len() && turn_ms - subs[*cursor] > SUBMIT_WINDOW_AFTER_MS {
            *cursor += 1;
        }
        let submit_ms = *subs.get(*cursor)?;
        let latency = turn_ms - submit_ms;
        if latency < -SUBMIT_WINDOW_BEFORE_MS {
            // The earliest live candidate is newer than turn+2s: nothing in
            // the window binds this turn, and later turns are newer still.
            return None;
        }
        *cursor += 1;
        self.bound += 1;
        self.latencies_ms.push(latency);
        Some(latency)
    }

    /// The receipt the fold reports over the `--days` window.
    /// `unwitnessed_sessions` is folded in by the caller (it needs the
    /// session rows).
    pub(crate) fn receipt(
        &self,
        window_start_ms: i64,
        unwitnessed_sessions: usize,
    ) -> WitnessReceipt {
        let submits = self
            .all_submit_ms
            .iter()
            .filter(|ms| **ms >= window_start_ms)
            .count();
        let unbound = submits.saturating_sub(self.bound);
        let mut latencies = self.latencies_ms.clone();
        latencies.sort_unstable();
        let latency_p95_ms = match latencies.len() {
            0 => None,
            n => Some(latencies[((n as f64) * 0.95).ceil() as usize - 1].max(0) as u64),
        };
        WitnessReceipt {
            submits,
            bound: self.bound,
            unbound,
            unresolved: self.unresolved,
            unwitnessed_sessions,
            latency_p95_ms,
        }
    }
}

/// Whether one `operator_submit` row names `session_id` with `submit_ms` in
/// `[since_ms, now_ms + SUBMIT_WINDOW_BEFORE_MS]`. The store's `since_ms`
/// pre-filter does not reach live-file lines, so the window is checked
/// here, on the row's own `submit_ms`. A read error answers false: no
/// witnessed submit, no arm.
pub(crate) fn submitted_since(
    journal: &Path,
    session_id: &str,
    since_ms: i64,
    now_ms: i64,
) -> bool {
    let query = crate::event_store::EventQuery {
        since_ms: Some(since_ms - 1_000),
        ..crate::event_store::EventQuery::of_types(&["operator_submit"])
    };
    let Ok(text) = crate::event_store::journal_text_checked(journal, &query) else {
        return false;
    };
    let wanted = crate::mail_hold::identity_key(session_id);
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("operator_submit") {
            continue;
        }
        if v.pointer("/data/resolution").and_then(|r| r.as_str()) != Some("ok") {
            continue;
        }
        let Some(session) = v.pointer("/data/harness_session").and_then(|s| s.as_str()) else {
            continue;
        };
        if crate::mail_hold::identity_key(session) != wanted {
            continue;
        }
        let Some(ms) = v.pointer("/data/submit_ms").and_then(|m| m.as_i64()) else {
            continue;
        };
        if (since_ms..=now_ms + SUBMIT_WINDOW_BEFORE_MS).contains(&ms) {
            return true;
        }
    }
    false
}

/// How far back an unfinished draft counts (C11): the newest
/// `operator_typing` row later than the newest submit holds mail this long,
/// even past the recent window, because a draft left mid-edit is exactly
/// what a delivery must not land over.
pub(crate) const DRAFT_MAX_MS: i64 = 600_000;

/// A typing row younger than this means the operator is typing right now.
pub(crate) const TYPING_RECENT_MS: i64 = 15_000;

/// Whether the operator is typing at `session_id` right now (`recent`) and
/// whether an unfinished draft sits in the composer (`draft`: the newest
/// typing row is later than the newest submit row). Both read the C11
/// `operator_typing` feed plus the submit witness since
/// `now_ms - DRAFT_MAX_MS`, under the same case rule as
/// [`submitted_since`]. A read error answers false/false, so a broken
/// journal never blocks mail.
pub(crate) struct TypingState {
    pub(crate) recent: bool,
    pub(crate) draft: bool,
    /// The newest `typed_ms` in the window, if any: the "typed since the
    /// inject" guard reads this directly.
    pub(crate) newest_typing_ms: Option<i64>,
}

pub(crate) fn typing_state(journal: &Path, session_id: &str, now_ms: i64) -> TypingState {
    let quiet = TypingState {
        recent: false,
        draft: false,
        newest_typing_ms: None,
    };
    let query = crate::event_store::EventQuery {
        since_ms: Some(now_ms - DRAFT_MAX_MS - 1_000),
        ..crate::event_store::EventQuery::of_types(&["operator_typing", "operator_submit"])
    };
    let Ok(text) = crate::event_store::journal_text_checked(journal, &query) else {
        return quiet;
    };
    let wanted = crate::mail_hold::identity_key(session_id);
    let mut newest_typing: Option<i64> = None;
    let mut newest_submit: Option<i64> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if kind != "operator_typing" && kind != "operator_submit" {
            continue;
        }
        if v.pointer("/data/resolution").and_then(|r| r.as_str()) != Some("ok") {
            continue;
        }
        let Some(session) = v.pointer("/data/harness_session").and_then(|s| s.as_str()) else {
            continue;
        };
        if crate::mail_hold::identity_key(session) != wanted {
            continue;
        }
        let field = if kind == "operator_typing" {
            "typed_ms"
        } else {
            "submit_ms"
        };
        let Some(ms) = v
            .pointer(&format!("/data/{field}"))
            .and_then(|m| m.as_i64())
        else {
            continue;
        };
        // The store's filter keys on the row envelope; the join key here is
        // typed_ms/submit_ms. Bound the draft window on the field itself.
        if ms < now_ms - DRAFT_MAX_MS {
            continue;
        }
        if kind == "operator_typing" {
            newest_typing = Some(newest_typing.map_or(ms, |t: i64| t.max(ms)));
        } else {
            newest_submit = Some(newest_submit.map_or(ms, |t: i64| t.max(ms)));
        }
    }
    TypingState {
        recent: newest_typing.is_some_and(|t| now_ms - t <= TYPING_RECENT_MS),
        draft: newest_typing.is_some_and(|t| newest_submit.is_none_or(|s| t > s)),
        newest_typing_ms: newest_typing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A unique scratch dir per call: parallel tests share one process pid,
    /// and one shared dir would let a rebuild race another test's load.
    fn test_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fno-witness-test-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn write_journal(path: &Path, rows: &[serde_json::Value]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut text = String::new();
        for row in rows {
            text.push_str(&row.to_string());
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }

    fn submit_row(session: Option<&str>, submit_ms: i64) -> serde_json::Value {
        serde_json::json!({
            "ts": "2026-09-21T18:00:00Z",
            "type": "operator_submit",
            "source": "daemon",
            "data": {
                "mux_session": "main",
                "pane": 7,
                "via": "pane",
                "submit_ms": submit_ms,
                "resolution": "ok",
                "harness_session": session,
            }
        })
    }

    #[test]
    fn load_keeps_only_ok_rows_with_a_session() {
        let dir = test_dir("load");
        let _ = std::fs::remove_dir_all(&dir);
        let journal = dir.join("events.jsonl");
        write_journal(
            &journal,
            &[
                submit_row(Some("s1"), 1_000),
                submit_row(Some("s1"), 2_000),
                submit_row(None, 3_000),
            ],
        );
        let index = SubmitIndex::load(&journal);
        assert_eq!(
            index.all_submit_ms.len(),
            3,
            "every operator_submit row counts"
        );
        let s1 = index.by_session.get("s1").unwrap();
        assert_eq!(s1.len(), 2, "only ok rows with a session join");
    }

    #[test]
    fn bind_consumes_one_submit_per_turn_inside_the_window() {
        let dir = test_dir("bind");
        let _ = std::fs::remove_dir_all(&dir);
        let journal = dir.join("events.jsonl");
        write_journal(&journal, &[submit_row(Some("s1"), 1_000)]);
        let mut index = SubmitIndex::load(&journal);
        assert_eq!(index.bind("s1", 1_500), Some(500));
        assert_eq!(index.bind("s1", 2_500), None, "one submit, one turn");
    }

    #[test]
    fn bind_refuses_a_submit_outside_the_window() {
        let dir = test_dir("refuse");
        let journal = dir.join("events.jsonl");
        write_journal(&journal, &[submit_row(Some("s1"), 1_000)]);
        let mut index = SubmitIndex::load(&journal);
        assert_eq!(index.bind("s1", 32_000), None, "31s is past the +30s bound");
    }

    /// One edge per bound: at `since_ms` (in), just below (out), inside the
    /// before-window (in), past it (out), and another session (never).
    /// Each edge row gets its own session, so one row's hit cannot mask
    /// another edge's miss.
    #[test]
    fn submitted_since_window_edges() {
        let dir = test_dir("submitted-since");
        let _ = std::fs::remove_dir_all(&dir);
        let journal = dir.join("events.jsonl");
        let now = 1_000_000i64;
        let since = now - SUBMIT_WINDOW_AFTER_MS;
        write_journal(
            &journal,
            &[
                submit_row(Some("s_at_bound"), since),
                submit_row(Some("s_below"), since - 1),
                submit_row(Some("s_inner"), now + 1_000),
                submit_row(Some("s_past"), now + SUBMIT_WINDOW_BEFORE_MS + 1),
                submit_row(Some("s_other"), now - 1_000),
            ],
        );
        assert!(submitted_since(&journal, "s_at_bound", since, now));
        assert!(
            !submitted_since(&journal, "s_below", since, now),
            "the row one ms below the window never binds"
        );
        assert!(submitted_since(&journal, "s_inner", now, now));
        assert!(
            !submitted_since(&journal, "s_past", now + 1_001, now),
            "a row past now + the before-window never binds"
        );
        assert!(
            !submitted_since(&journal, "s_at_bound", now - 2_000, now),
            "another session's in-window row never binds"
        );
        assert!(!submitted_since(&journal, "missing", since, now));
    }

    fn typing_row(session: Option<&str>, typed_ms: i64) -> serde_json::Value {
        serde_json::json!({
            "ts": "2026-09-27T18:00:00Z",
            "type": "operator_typing",
            "source": "daemon",
            "data": {
                "mux_session": "main",
                "pane": 7,
                "via": "pane",
                "typed_ms": typed_ms,
                "resolution": "ok",
                "harness_session": session,
            }
        })
    }

    #[test]
    fn typing_state_reads_recent_and_draft_from_the_feed() {
        let dir = test_dir("typing");
        let _ = std::fs::remove_dir_all(&dir);
        let journal = dir.join("events.jsonl");
        let now = 1_000_000i64;
        // Typing 3s ago, submit 10s ago: typing now AND an open draft.
        write_journal(
            &journal,
            &[
                submit_row(Some("s1"), now - 10_000),
                typing_row(Some("s1"), now - 3_000),
            ],
        );
        let ts = typing_state(&journal, "s1", now);
        assert!(ts.recent && ts.draft);
        // A submit AFTER the typing row closes the draft; the 8s-old typing
        // row is still recent, so only the draft flag clears.
        write_journal(
            &journal,
            &[
                submit_row(Some("s2"), now - 10_000),
                typing_row(Some("s2"), now - 8_000),
                submit_row(Some("s2"), now - 1_000),
            ],
        );
        let ts = typing_state(&journal, "s2", now);
        assert!(ts.recent && !ts.draft, "a later submit ends the draft");
        // Old typing with a later submit: neither recent nor draft, so the
        // quiet gate proceeds.
        write_journal(
            &journal,
            &[
                typing_row(Some("s6"), now - 16_000),
                submit_row(Some("s6"), now - 1_000),
            ],
        );
        let ts = typing_state(&journal, "s6", now);
        assert!(!ts.recent && !ts.draft);
        // A draft older than DRAFT_MAX_MS ages out entirely.
        write_journal(
            &journal,
            &[typing_row(Some("s3"), now - DRAFT_MAX_MS - 5_000)],
        );
        let ts = typing_state(&journal, "s3", now);
        assert!(!ts.recent && !ts.draft, "a stale draft does not block");
        // Another session's typing never binds.
        write_journal(&journal, &[typing_row(Some("s4"), now - 1_000)]);
        let ts = typing_state(&journal, "s5", now);
        assert!(!ts.recent && !ts.draft);
    }

    #[test]
    fn typing_state_survives_a_broken_journal() {
        let dir = test_dir("typing-broken");
        let _ = std::fs::remove_dir_all(&dir);
        let journal = dir.join("missing");
        let ts = typing_state(&journal, "s1", 1_000_000);
        assert!(
            !ts.recent && !ts.draft,
            "a broken journal never blocks mail"
        );
    }
}
