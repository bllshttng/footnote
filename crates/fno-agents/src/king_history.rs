//! `king-history`: the crown-scope `reign_checkin` readback behind
//! `fno agents king history`.
//!
//! Python resolves the caller's crown scope (harness identity and registry
//! rows are Python-owned), passes every journal `paths.event_journals`
//! returns, and relays here. The paths reduce to unique live journals (the
//! `.ephemeral` siblings never carry durable rows, and a `.1` generation is
//! the same journal its live path names); each live journal's `events.db`
//! store is synced FIRST (`events_store::sync`, which ingests the rotated
//! generation and then the live file), and the read is an indexed
//! `(scope, type, ts_ms)` select instead of a scan of every row ever
//! journaled. Selection is EXACT `data.scope` equality via the store's
//! `scope` column: rows are written through the crown canonicalization, so
//! a second normalizer here could only disagree with it; a stored row whose
//! `data.scope` was not canonical carries `scope IS NULL` and reaches the
//! legacy classifier through the same query. Legacy rows (the refused
//! `crown_scope`/`crown`/`result` aliases, or a missing canonical key)
//! stay byte-preserved evidence: counted in `rejected`, listed in
//! `rejected_legacy` by store and `ts`, because a line number does not
//! survive rotation. The output is read-back, never a generated summary; a
//! zero-match answer still names every store and its counts, so an empty
//! history is a measurement, not an absence. A store that cannot be opened
//! is an error (rc 1), never an empty history.

use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) const REIGN_CHECKIN: &str = "reign_checkin";
pub(crate) const FORBIDDEN_ALIASES: [&str; 3] = ["crown", "crown_scope", "result"];

fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// One stored row's verdict: matched, rejected (with optional legacy
/// evidence when one of its scope spellings names the requested crown).
/// Shared by scope-stamped and NULL-scope result sets, so the canonical and
/// legacy tests apply identically to both.
fn classify(event: &Value, scope: &str) -> (bool, bool, Option<Value>) {
    let Some(data) = event.get("data").and_then(|d| d.as_object()) else {
        // A reign_checkin without an object payload is legacy evidence
        // too; it names no scope, so it counts but attributes nowhere.
        return (false, true, None);
    };
    let data = Value::Object(data.clone());
    let aliases: Vec<&str> = FORBIDDEN_ALIASES
        .iter()
        .filter(|k| data.get(**k).is_some())
        .copied()
        .collect();
    let row_scope = s_str(&data, "scope").unwrap_or("");
    let canonical = !row_scope.is_empty() && data.get("change").is_some() && aliases.is_empty();
    if canonical {
        return (row_scope == scope, false, None);
    }
    let names_this_crown =
        row_scope == scope || aliases.iter().any(|k| s_str(&data, k) == Some(scope));
    let legacy = names_this_crown.then(|| {
        let missing: Vec<&str> = ["scope", "change"]
            .iter()
            .filter(|k| data.get(**k).is_none())
            .copied()
            .collect();
        json!({
            "forbidden": aliases,
            "missing": missing,
        })
    });
    (false, true, legacy)
}

/// The stored reign rows for one store: exact-scope rows, then NULL-scope
/// rows (non-canonical scope spellings), each oldest first within its set.
fn reign_rows(store: &Connection, scope: &str) -> Result<Vec<String>, String> {
    let mut rows: Vec<String> = Vec::new();
    let read = |stmt: &mut rusqlite::Statement,
                args: &[&dyn rusqlite::ToSql],
                rows: &mut Vec<String>|
     -> Result<(), String> {
        let found = stmt
            .query_map(args, |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows.extend(found);
        Ok(())
    };
    let mut scoped = store
        .prepare("SELECT line FROM events WHERE scope = ?1 AND type = ?2 ORDER BY ts_ms")
        .map_err(|e| e.to_string())?;
    read(&mut scoped, &[&scope, &REIGN_CHECKIN], &mut rows)?;
    let mut legacy = store
        .prepare("SELECT line FROM events WHERE scope IS NULL AND type = ?1 ORDER BY ts_ms")
        .map_err(|e| e.to_string())?;
    read(&mut legacy, &[&REIGN_CHECKIN], &mut rows)?;
    Ok(rows)
}

pub(crate) fn scan(events_paths: &[PathBuf], scope: &str) -> Result<Value, String> {
    // Generations and mirrors collapse here: one live journal, one store.
    let mut lives: Vec<PathBuf> = Vec::new();
    for path in events_paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(crate::events::EPHEMERAL_SUFFIX) {
            continue;
        }
        let live = crate::events_store::live_journal(path);
        if !lives.contains(&live) {
            lives.push(live);
        }
    }
    let mut payload = json!({
        "scope": scope,
        "journals": Vec::<Value>::new(),
        "scanned": 0,
        "matched": 0,
        "events": Vec::<Value>::new(),
        "rejected": 0,
        "rejected_legacy": Vec::<Value>::new(),
        "duplicates": 0,
        "ingested": 0,
    });
    let mut events: Vec<Value> = Vec::new();
    let mut rejected_legacy: Vec<Value> = Vec::new();
    let mut journals: Vec<Value> = Vec::new();
    // The loop runtime mirrors rows across journals, so one check-in can sit
    // in two files. The history lists a check-in once; the collapsed copies
    // are counted, not silently dropped.
    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicates: u64 = 0;
    for live in &lives {
        let receipt = crate::events_store::sync(live)?;
        let store = crate::events_store::open_read(&receipt.store)?;
        let rows = reign_rows(&store, scope)?;
        let mut scanned = 0u64;
        let mut matched = 0u64;
        let mut rejected = 0u64;
        for line in rows {
            scanned += 1;
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let (hit, rejected_row, legacy) = classify(&event, scope);
            let ts_val = event.get("ts").cloned().unwrap_or(Value::Null);
            if hit {
                matched += 1;
                let key = serde_json::to_string(&event).unwrap_or_default();
                if seen.insert(key) {
                    events.push(event);
                } else {
                    duplicates += 1;
                }
            }
            if rejected_row {
                rejected += 1;
                if let Some(mut detail) = legacy {
                    detail["file"] = json!(receipt.store.display().to_string());
                    detail["ts"] = ts_val;
                    rejected_legacy.push(detail);
                }
            }
        }
        journals.push(json!({
            "path": live.display().to_string(),
            "store": receipt.store.display().to_string(),
            "ingested": receipt.ingested,
            "corrupt": receipt.corrupt,
            "scanned": scanned,
            "matched": matched,
            "rejected": rejected,
        }));
        payload["scanned"] = json!(payload["scanned"].as_u64().unwrap_or(0) + scanned);
        payload["rejected"] = json!(payload["rejected"].as_u64().unwrap_or(0) + rejected);
        payload["ingested"] = json!(payload["ingested"].as_u64().unwrap_or(0) + receipt.ingested);
    }
    // Across rotations one reign spans several files, so file order is no
    // longer display order; the envelope's own ts is.
    events.sort_by(|a, b| {
        s_str(b, "ts")
            .unwrap_or("")
            .cmp(s_str(a, "ts").unwrap_or(""))
    });
    payload["events"] = Value::Array(events);
    // The store returns the scoped set before the NULL-scope set; evidence
    // reads chronologically, so the legacy rows sort by their own ts.
    rejected_legacy.sort_by(|a, b| {
        s_str(a, "ts")
            .unwrap_or("")
            .cmp(s_str(b, "ts").unwrap_or(""))
    });
    payload["rejected_legacy"] = Value::Array(rejected_legacy);
    payload["journals"] = Value::Array(journals);
    payload["duplicates"] = json!(duplicates);
    payload["matched"] = json!(payload["events"].as_array().map(|a| a.len()).unwrap_or(0));
    Ok(payload)
}

fn render(payload: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in payload["events"].as_array().unwrap() {
        let data = event.get("data").cloned().unwrap_or_else(|| json!({}));
        lines.push(format!(
            "{}  {}",
            s_str(event, "ts").unwrap_or(""),
            s_str(&data, "scope").unwrap_or("")
        ));
        lines.push(format!(
            "  change: {}",
            s_str(&data, "change").unwrap_or("")
        ));
        if let Some(rest) = data.as_object() {
            let rest: serde_json::Map<String, Value> = rest
                .iter()
                .filter(|(k, _)| k.as_str() != "scope" && k.as_str() != "change")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                lines.push(format!(
                    "  evidence: {}",
                    serde_json::to_string(&rest).unwrap_or_default()
                ));
            }
        }
    }
    lines.push(format!(
        "history: {} canonical check-in(s) for {}, read {} reign row(s) from {} store(s), {} row(s) ingested, {} legacy-invalid reign row(s)",
        payload["matched"], payload["scope"], payload["scanned"],
        payload["journals"].as_array().map(|a| a.len()).unwrap_or(0),
        payload["ingested"],
        payload["rejected"]
    ));
    if payload["duplicates"].as_u64().unwrap_or(0) > 0 {
        lines.push(format!(
            "  ({} duplicate mirror row(s) collapsed)",
            payload["duplicates"]
        ));
    }
    for journal in payload["journals"].as_array().unwrap() {
        lines.push(format!(
            "  {}: store {}, {} row(s) ingested, {} reign row(s) read, {} matched",
            journal["path"],
            journal["store"],
            journal["ingested"],
            journal["scanned"],
            journal["matched"]
        ));
    }
    for entry in payload["rejected_legacy"].as_array().unwrap() {
        lines.push(format!(
            "  rejected legacy row in {} at {}: forbidden={} missing={}",
            entry["file"],
            entry["ts"].as_str().unwrap_or(""),
            serde_json::to_string(&entry["forbidden"]).unwrap_or_default(),
            serde_json::to_string(&entry["missing"]).unwrap_or_default(),
        ));
    }
    lines.join("\n")
}

/// `king-history --scope SCOPE --events-path PATH [--events-path PATH ...] [--json]`
///
/// rc 0 read (any match count), 1 a store that cannot be opened or synced
/// (the message names the store path), 2 usage failure.
pub fn run_king_history(args: &[String]) -> i32 {
    let mut scope = String::new();
    let mut events_paths: Vec<PathBuf> = Vec::new();
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" if i + 1 < args.len() => {
                scope = args[i + 1].clone();
                i += 2;
            }
            "--events-path" if i + 1 < args.len() => {
                events_paths.push(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--json" => {
                as_json = true;
                i += 1;
            }
            other => {
                eprintln!("fno-agents king-history: unknown flag {other}");
                eprintln!(
                    "fno-agents king-history: --scope SCOPE --events-path PATH \
                     [--events-path PATH ...] [--json]"
                );
                return 2;
            }
        }
    }
    if scope.is_empty() || events_paths.is_empty() {
        eprintln!("fno-agents king-history: --scope and --events-path are required");
        return 2;
    }
    match scan(&events_paths, &scope) {
        Ok(payload) => {
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!("{}", render(&payload));
            }
            0
        }
        Err(msg) => {
            eprintln!("fno-agents king-history: {msg}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn journal(rows: &[Value]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        for row in rows {
            writeln!(fh, "{row}").unwrap();
        }
        (dir, path)
    }

    fn checkin(ts: &str, data: Value) -> Value {
        json!({"ts": ts, "type": "reign_checkin", "source": "loop", "data": data})
    }

    #[test]
    fn newest_first_with_evidence_intact() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"scope": "x-a792", "change": "first"}),
            ),
            json!({"ts": "2026-09-10T09:00:00Z", "type": "phase_transition", "source": "loop", "data": {"phase": "review"}}),
            checkin(
                "2026-09-10T09:30:00Z",
                json!({"scope": "other", "change": "elsewhere"}),
            ),
            checkin(
                "2026-09-10T12:00:00Z",
                json!({"scope": "x-a792", "change": "merged PR 1710", "open_prs_fleet": 3}),
            ),
        ]);
        let payload = scan(std::slice::from_ref(&path), "x-a792").unwrap();
        assert_eq!(payload["scanned"], json!(2), "only reign rows are read");
        assert_eq!(payload["matched"], json!(2));
        let events = payload["events"].as_array().unwrap();
        assert_eq!(events[0]["data"]["open_prs_fleet"], json!(3));
        assert_eq!(events[0]["ts"], json!("2026-09-10T12:00:00Z"));
        assert_eq!(events[1]["data"]["change"], json!("first"));
    }

    #[test]
    fn alias_rows_are_rejected_evidence_attributed_by_line() {
        let (_dir, path) = journal(&[
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"crown_scope": "x-a792", "change": "old"}),
            ),
            checkin(
                "2026-09-10T08:30:00Z",
                json!({"scope": "x-a792", "result": "no change"}),
            ),
            checkin("2026-09-10T09:00:00Z", json!({"change": "no scope named"})),
        ]);
        let payload = scan(std::slice::from_ref(&path), "x-a792").unwrap();
        assert_eq!(payload["matched"], json!(0));
        assert_eq!(payload["rejected"], json!(3));
        let legacy = payload["rejected_legacy"].as_array().unwrap();
        assert_eq!(legacy.len(), 2);
        assert_eq!(legacy[0]["forbidden"], json!(["crown_scope"]));
        assert_eq!(legacy[0]["missing"], json!(["scope"]));
        assert_eq!(legacy[1]["forbidden"], json!(["result"]));
    }

    #[test]
    fn missing_journal_reads_as_positive_zero() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.jsonl");
        let payload = scan(std::slice::from_ref(&absent), "x-a792").unwrap();
        assert_eq!(payload["scanned"], json!(0));
        assert_eq!(payload["matched"], json!(0));
        let journals = payload["journals"].as_array().unwrap();
        assert_eq!(journals.len(), 1);
        assert!(journals[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("absent.jsonl"));
        assert_eq!(journals[0]["scanned"], json!(0));
    }

    #[test]
    fn corrupt_line_is_stored_and_history_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&path).unwrap();
        writeln!(fh, "{{not json").unwrap();
        drop(fh);
        // A corrupt line is stored with its reject_reason; it is never a
        // reign row, so the read succeeds and reports zero.
        let payload = scan(std::slice::from_ref(&path), "x-a792").unwrap();
        assert_eq!(payload["scanned"], json!(0));
        assert_eq!(payload["matched"], json!(0));
        let journals = payload["journals"].as_array().unwrap();
        assert_eq!(journals[0]["corrupt"], json!(1));
    }

    #[test]
    fn newest_first_across_rotations() {
        let dir = tempfile::tempdir().unwrap();
        let rotated = dir.path().join("events.jsonl.1");
        let live = dir.path().join("events.jsonl");
        let mut fh = std::fs::File::create(&rotated).unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-09T08:00:00Z",
                json!({"scope": "x-a792", "change": "rotated past"}),
            )
        )
        .unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"scope": "x-a792", "change": "older"}),
            )
        )
        .unwrap();
        drop(fh);
        let mut fh = std::fs::File::create(&live).unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-10T12:00:00Z",
                json!({"scope": "x-a792", "change": "newest"}),
            )
        )
        .unwrap();
        drop(fh);
        let payload = scan(&[rotated, live], "x-a792").unwrap();
        assert_eq!(payload["scanned"], json!(3));
        assert_eq!(payload["matched"], json!(3));
        let events = payload["events"].as_array().unwrap();
        assert_eq!(events[0]["data"]["change"], json!("newest"));
        assert_eq!(events[1]["data"]["change"], json!("older"));
        assert_eq!(events[2]["data"]["change"], json!("rotated past"));
        let journals = payload["journals"].as_array().unwrap();
        // Generations collapse: one live journal, one store entry naming it.
        assert_eq!(journals.len(), 1);
        assert!(journals[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("events.jsonl"));
        assert!(journals[0]["store"]
            .as_str()
            .unwrap()
            .ends_with("events.db"));
        assert_eq!(journals[0]["ingested"], json!(3));
        assert_eq!(journals[0]["scanned"], json!(3));
        assert_eq!(journals[0]["matched"], json!(3));
    }

    #[test]
    fn mirrored_duplicate_collapses_and_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let space = dir.path().join("events.jsonl");
        let mirror = dir.path().join("global.jsonl");
        let row = checkin(
            "2026-09-10T12:00:00Z",
            json!({"scope": "x-a792", "change": "mirrored"}),
        );
        for path in [&space, &mirror] {
            let mut fh = std::fs::File::create(path).unwrap();
            writeln!(fh, "{row}").unwrap();
            drop(fh);
        }
        let payload = scan(&[space, mirror], "x-a792").unwrap();
        assert_eq!(payload["matched"], json!(1));
        assert_eq!(payload["duplicates"], json!(1));
    }

    #[test]
    fn legacy_rows_name_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let rotated = dir.path().join("events.jsonl.1");
        let mut fh = std::fs::File::create(&rotated).unwrap();
        writeln!(
            fh,
            "{}",
            checkin(
                "2026-09-10T08:00:00Z",
                json!({"crown_scope": "x-a792", "change": "old"}),
            )
        )
        .unwrap();
        drop(fh);
        let payload = scan(std::slice::from_ref(&rotated), "x-a792").unwrap();
        let legacy = payload["rejected_legacy"].as_array().unwrap();
        assert_eq!(legacy.len(), 1);
        assert!(
            legacy[0]["file"].as_str().unwrap().ends_with("events.db"),
            "a line number does not survive rotation; the store does"
        );
        assert_eq!(legacy[0]["ts"], json!("2026-09-10T08:00:00Z"));
    }

    #[test]
    fn unreadable_store_errors_and_names_it() {
        // AC4-ERR: a blocked store is an error, never an empty history.
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("events.jsonl");
        std::fs::create_dir(dir.path().join("events.db")).unwrap();
        let err = scan(std::slice::from_ref(&live), "x-a792").unwrap_err();
        assert!(err.contains("events.db"), "err: {err}");
    }

    #[test]
    fn run_relays_json_and_human_formats() {
        let (_dir, path) = journal(&[checkin(
            "2026-09-10T12:00:00Z",
            json!({"scope": "x-a792", "change": "did a thing", "open_prs_fleet": 3}),
        )]);
        let args = vec![
            "--scope".to_string(),
            "x-a792".to_string(),
            "--events-path".to_string(),
            path.display().to_string(),
            "--json".to_string(),
        ];
        assert_eq!(run_king_history(&args), 0);
        let text_args = args.clone();
        assert_eq!(run_king_history(&text_args), 0);
    }

    #[test]
    fn usage_failure_exit_two() {
        assert_eq!(run_king_history(&[]), 2);
        assert_eq!(
            run_king_history(&["--scope".to_string(), "x".to_string()]),
            2
        );
        assert_eq!(run_king_history(&["--nope".to_string()]), 2);
    }
}
